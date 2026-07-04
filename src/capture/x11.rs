//! X11 screen-capture backend using `x11rb`, with an MIT-SHM (XShm) fast path.
//!
//! This backend captures either the **root window** (whole-monitor capture) or a
//! **single window** at a fixed framerate, delivering each grab to the
//! [`FrameSink`] as a [`Frame`]. The target is chosen by [`CaptureTarget`]:
//!
//! * [`CaptureTarget::Monitor`] grabs the root window (the original behavior).
//! * [`CaptureTarget::Window`] grabs a specific window, re-found across launches
//!   by its `WM_CLASS`/title (persisted under `~/.config/rewind/window.target`) —
//!   this is the X11 "re-attach to the same window" behavior the origin thread
//!   misses on Wayland.
//! * [`CaptureTarget::ActiveWindow`] resolves `_NET_ACTIVE_WINDOW` at start and
//!   captures whatever is focused (also remembering it for `Window` mode).
//!
//! Window capture uses **XComposite** (`RedirectAutomatic` + `NameWindowPixmap`)
//! so occluded or partially off-screen windows still render into an off-screen
//! pixmap we grab from. If the compositor already redirects the window (the usual
//! case on a compositing desktop) we reuse that; otherwise we redirect it
//! ourselves and undo it on teardown. Without the extension we fall back to
//! grabbing the window directly (visible region only).
//!
//! Two capture paths exist:
//!
//! * **XShm** (fast, zero socket copy): the server writes the captured image
//!   straight into a POSIX shared-memory segment this process maps. Attaching a
//!   SysV shm segment requires the libc `shmget`/`shmat`/`shmctl`/`shmdt` calls,
//!   which `x11rb` intentionally does not wrap. The `capture-x11` feature pulls in
//!   the `libc` crate for exactly this purpose, so the SHM fast path is compiled
//!   whenever this module is (`#[cfg(feature = "capture-x11")]`).
//! * **`xproto::get_image`** (portable fallback): pixels travel back inside the
//!   reply over the X socket. Always compiled; correct everywhere, just slower.
//!
//! Both paths request a 32-bit ZPixmap of the target drawable.
//!
//! Pixel format: on the overwhelmingly common little-endian X server with a
//! 24/32-bit TrueColor visual, a 32-bit ZPixmap image is laid out in memory as
//! `B, G, R, X` per pixel → [`PixelFormat::Bgrx8888`]. We inspect the target
//! visual's RGB masks and the server byte order to pick the format, defaulting to
//! Bgrx on anything unusual.
//!
//! ## Not yet handled (TODO)
//! - **Hardware cursor compositing.** `get_image` omits the cursor sprite; it
//!   needs `xfixes::get_cursor_image` + manual alpha-blend.
//! - **RandR multi-monitor.** Monitor capture grabs the full virtual screen;
//!   cropping to one output needs `randr::get_monitors`.
//! - **Live geometry changes.** The captured size is sampled once at `start()`.
//!   If a captured window resizes we re-name its composite pixmap so content
//!   stays fresh, but keep emitting frames at the original dimensions (larger
//!   windows are cropped to the top-left; smaller ones skip the frame until they
//!   grow back), since the encoder resolution is fixed at the first frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::capture::{CaptureError, FrameSink, FrameSource};
use crate::media::{CaptureTarget, Frame, PixelFormat, StreamInfo};

/// Default capture rate in frames per second when nothing else is negotiated.
const DEFAULT_FRAMERATE: u32 = 60;

/// Bytes per pixel for the 32-bit ZPixmap formats we support.
const BPP: u32 = 4;

impl From<x11rb::errors::ConnectionError> for CaptureError {
    fn from(e: x11rb::errors::ConnectionError) -> Self {
        CaptureError::Backend(format!("x11 connection error: {e}"))
    }
}

impl From<x11rb::errors::ReplyError> for CaptureError {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        CaptureError::Backend(format!("x11 reply error: {e}"))
    }
}

impl From<x11rb::errors::ReplyOrIdError> for CaptureError {
    fn from(e: x11rb::errors::ReplyOrIdError) -> Self {
        CaptureError::Backend(format!("x11 reply/id error: {e}"))
    }
}

/// Open the X11 capture backend for `target`.
///
/// Connects to the X server named by `$DISPLAY`, selects the default screen and
/// its root window, resolves the target drawable (root or a specific window),
/// samples its geometry, chooses a pixel format, and (when the SHM path is
/// compiled in) probes for the MIT-SHM extension. The returned source is *not*
/// running yet — call [`FrameSource::start`], which sets up the XComposite
/// redirect for window targets.
pub fn open(target: CaptureTarget) -> Result<Box<dyn FrameSource>, CaptureError> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| CaptureError::Unsupported(format!("cannot connect to X server: {e}")))?;

    let setup_owned = conn.setup().clone();
    let screen = setup_owned
        .roots
        .get(screen_num)
        .ok_or_else(|| CaptureError::Backend(format!("no screen at index {screen_num}")))?;
    let root = screen.root;
    let root_depth = screen.root_depth;
    let root_visual = screen.root_visual;

    // Resolve which window (if any) we capture, remembering it for next launch.
    let mode = resolve_target(&conn, root, target)?;

    // Sample the target geometry now so callers can read stream_info() before
    // start(); pick the visual for pixel-format detection from the target.
    let (width, height, depth, visual) = match mode {
        TargetMode::Root => {
            let geom = conn
                .get_geometry(root)?
                .reply()
                .map_err(|e| CaptureError::Backend(format!("get_geometry(root) failed: {e}")))?;
            (geom.width as u32, geom.height as u32, root_depth, root_visual)
        }
        TargetMode::Window(win) => {
            let geom = conn
                .get_geometry(win)?
                .reply()
                .map_err(|e| CaptureError::Backend(format!("get_geometry(window) failed: {e}")))?;
            let vis = window_visual(&conn, win).unwrap_or(root_visual);
            (geom.width as u32, geom.height as u32, geom.depth, vis)
        }
    };

    if width == 0 || height == 0 {
        return Err(CaptureError::Backend(
            "capture target reports zero size".into(),
        ));
    }

    let format = detect_format(&setup_owned, visual);

    // Probe SHM only if the SHM path is even compiled in (needs libc).
    let has_shm = shm_available(&conn);

    let info = StreamInfo {
        width,
        height,
        framerate: DEFAULT_FRAMERATE,
        format,
    };

    let name = match (mode, has_shm) {
        (TargetMode::Root, true) => "x11-xshm",
        (TargetMode::Root, false) => "x11-getimage",
        (TargetMode::Window(_), true) => "x11-window-xshm",
        (TargetMode::Window(_), false) => "x11-window-getimage",
    };

    Ok(Box::new(X11Source {
        name,
        conn: Some(Arc::new(conn)),
        root,
        mode,
        depth,
        has_shm,
        info: Arc::new(Mutex::new(info)),
        running: Arc::new(AtomicBool::new(false)),
        thread: None,
    }))
}

/// The resolved capture target: the whole root, or a specific window id.
#[derive(Debug, Clone, Copy)]
enum TargetMode {
    Root,
    Window(xproto::Window),
}

/// Map a [`CaptureTarget`] to a concrete window, persisting the chosen window's
/// identity so [`CaptureTarget::Window`] re-attaches to it on the next launch.
fn resolve_target(
    conn: &RustConnection,
    root: xproto::Window,
    target: CaptureTarget,
) -> Result<TargetMode, CaptureError> {
    match target {
        CaptureTarget::Monitor => Ok(TargetMode::Root),
        CaptureTarget::ActiveWindow => {
            let win = active_window(conn, root)?;
            save_window_descriptor(&describe_window(conn, win));
            Ok(TargetMode::Window(win))
        }
        CaptureTarget::Window => {
            // Prefer the remembered window; fall back to the active one so a
            // first run (nothing saved yet) still captures something sensible.
            let win = load_window_descriptor()
                .and_then(|desc| find_window(conn, root, &desc))
                .or_else(|| active_window(conn, root).ok())
                .ok_or_else(|| {
                    CaptureError::Backend(
                        "no window to capture: none remembered and no active window".into(),
                    )
                })?;
            save_window_descriptor(&describe_window(conn, win));
            Ok(TargetMode::Window(win))
        }
    }
}

// --- window identification -----------------------------------------------------

/// Intern an atom, returning `None` if the server doesn't know it.
fn intern(conn: &RustConnection, name: &[u8]) -> Option<xproto::Atom> {
    conn.intern_atom(true, name)
        .ok()?
        .reply()
        .ok()
        .map(|r| r.atom)
        .filter(|a| *a != 0)
}

/// The window id in `_NET_ACTIVE_WINDOW` on the root, or an error if there is no
/// active window (EWMH not supported, or nothing focused).
fn active_window(conn: &RustConnection, root: xproto::Window) -> Result<xproto::Window, CaptureError> {
    let atom = intern(conn, b"_NET_ACTIVE_WINDOW").ok_or_else(|| {
        CaptureError::Backend("_NET_ACTIVE_WINDOW unsupported (no EWMH window manager?)".into())
    })?;
    let reply = conn
        .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)?
        .reply()
        .map_err(|e| CaptureError::Backend(format!("read _NET_ACTIVE_WINDOW: {e}")))?;
    let win = reply.value32().and_then(|mut it| it.next()).unwrap_or(0);
    if win == 0 {
        return Err(CaptureError::Backend(
            "no active window to capture (_NET_ACTIVE_WINDOW is None)".into(),
        ));
    }
    Ok(win)
}

/// A window's `WM_CLASS` (instance, class) and title, used to re-find it later.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct WindowDescriptor {
    class: Option<String>,
    instance: Option<String>,
    title: Option<String>,
}

fn describe_window(conn: &RustConnection, win: xproto::Window) -> WindowDescriptor {
    let (instance, class) = wm_class(conn, win);
    WindowDescriptor {
        class,
        instance,
        title: window_title(conn, win),
    }
}

/// Read `WM_CLASS` → `(instance, class)`. The property is two NUL-terminated
/// strings: the instance name followed by the class name.
fn wm_class(conn: &RustConnection, win: xproto::Window) -> (Option<String>, Option<String>) {
    let reply = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()
        .and_then(|c| c.reply().ok());
    let Some(reply) = reply else {
        return (None, None);
    };
    let mut parts = reply.value.split(|b| *b == 0);
    let instance = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let class = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned());
    (instance, class)
}

/// Read a window title, preferring `_NET_WM_NAME` (UTF-8) over legacy `WM_NAME`.
fn window_title(conn: &RustConnection, win: xproto::Window) -> Option<String> {
    if let Some(net_name) = intern(conn, b"_NET_WM_NAME") {
        let utf8: xproto::Atom = intern(conn, b"UTF8_STRING").unwrap_or_else(|| AtomEnum::STRING.into());
        if let Some(s) = get_text_property(conn, win, net_name, utf8) {
            return Some(s);
        }
    }
    get_text_property(conn, win, AtomEnum::WM_NAME, AtomEnum::STRING)
}

fn get_text_property(
    conn: &RustConnection,
    win: xproto::Window,
    prop: impl Into<xproto::Atom>,
    ty: impl Into<xproto::Atom>,
) -> Option<String> {
    let reply = conn
        .get_property(false, win, prop, ty, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    let s = String::from_utf8_lossy(&reply.value)
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The window's visual id (for pixel-format detection), or `None` on failure.
fn window_visual(conn: &RustConnection, win: xproto::Window) -> Option<xproto::Visualid> {
    conn.get_window_attributes(win)
        .ok()?
        .reply()
        .ok()
        .map(|a| a.visual)
}

/// Re-find the window matching a saved descriptor. Requires the `WM_CLASS` class
/// to match; the title and instance break ties (so relaunching the same app
/// picks the same window even if several are open).
fn find_window(
    conn: &RustConnection,
    root: xproto::Window,
    desc: &WindowDescriptor,
) -> Option<xproto::Window> {
    let candidates = client_list(conn, root).unwrap_or_else(|| tree_windows(conn, root));
    let mut best: Option<(u32, xproto::Window)> = None;
    for win in candidates {
        let d = describe_window(conn, win);
        let class_matches = match (&d.class, &desc.class) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if !class_matches {
            continue;
        }
        let mut score = 2;
        if desc.title.is_some() && d.title == desc.title {
            score += 2;
        }
        if desc.instance.is_some() && d.instance == desc.instance {
            score += 1;
        }
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, win));
        }
    }
    best.map(|(_, w)| w)
}

/// The WM-maintained `_NET_CLIENT_LIST` of managed top-level windows.
fn client_list(conn: &RustConnection, root: xproto::Window) -> Option<Vec<xproto::Window>> {
    let atom = intern(conn, b"_NET_CLIENT_LIST")?;
    let reply = conn
        .get_property(false, root, atom, AtomEnum::WINDOW, 0, u32::MAX)
        .ok()?
        .reply()
        .ok()?;
    let list: Vec<xproto::Window> = reply.value32()?.collect();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// Fallback when EWMH `_NET_CLIENT_LIST` is unavailable: the root's direct
/// children and one level below (to reach reparented client windows).
fn tree_windows(conn: &RustConnection, root: xproto::Window) -> Vec<xproto::Window> {
    let mut out = Vec::new();
    let Some(top) = conn.query_tree(root).ok().and_then(|c| c.reply().ok()) else {
        return out;
    };
    for &child in &top.children {
        out.push(child);
        if let Some(sub) = conn.query_tree(child).ok().and_then(|c| c.reply().ok()) {
            out.extend(sub.children);
        }
    }
    out
}

// --- window-target persistence -------------------------------------------------

/// `${XDG_CONFIG_HOME:-~/.config}/rewind/window.target`.
fn target_file() -> Option<std::path::PathBuf> {
    Some(crate::capture::config_dir()?.join("window.target"))
}

/// Persist the chosen window's identity as `key=value` lines so a later
/// [`CaptureTarget::Window`] launch can re-find it. Best-effort.
fn save_window_descriptor(desc: &WindowDescriptor) {
    let Some(path) = target_file() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format_descriptor(desc));
}

fn load_window_descriptor() -> Option<WindowDescriptor> {
    let text = std::fs::read_to_string(target_file()?).ok()?;
    parse_descriptor(&text)
}

/// Serialize a descriptor to `key=value` lines. Values are single-line, so any
/// newline in a title is folded to a space to keep the format round-trippable.
fn format_descriptor(desc: &WindowDescriptor) -> String {
    let clean = |s: &str| s.replace(['\n', '\r'], " ");
    let mut out = String::new();
    if let Some(c) = &desc.class {
        out.push_str(&format!("class={}\n", clean(c)));
    }
    if let Some(i) = &desc.instance {
        out.push_str(&format!("instance={}\n", clean(i)));
    }
    if let Some(t) = &desc.title {
        out.push_str(&format!("title={}\n", clean(t)));
    }
    out
}

/// Parse `key=value` lines back into a descriptor. Unknown keys are ignored; an
/// empty/unrecognized document yields `None` (nothing to re-attach to). A `=` in
/// a value is preserved (only the first `=` splits the line).
fn parse_descriptor(text: &str) -> Option<WindowDescriptor> {
    let mut desc = WindowDescriptor::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "class" => desc.class = Some(value.to_string()),
            "instance" => desc.instance = Some(value.to_string()),
            "title" => desc.title = Some(value.to_string()),
            _ => {}
        }
    }
    if desc == WindowDescriptor::default() {
        None
    } else {
        Some(desc)
    }
}

// --- XComposite window grab ----------------------------------------------------

/// Where frames are grabbed from, plus the state to keep a window's off-screen
/// pixmap fresh and to undo any redirect we installed.
enum GrabSource {
    /// Whole root window (monitor capture).
    Root { root: xproto::Window },
    /// A window captured through its XComposite off-screen pixmap.
    Window {
        window: xproto::Window,
        pixmap: xproto::Pixmap,
        /// True if we installed the redirect (no compositor had), so we undo it.
        redirected_by_us: bool,
    },
    /// A window with XComposite unavailable — grabbed directly (visible region).
    WindowDirect { window: xproto::Window },
}

impl GrabSource {
    /// The drawable to grab this iteration.
    fn drawable(&self) -> xproto::Drawable {
        match self {
            GrabSource::Root { root } => *root,
            GrabSource::Window { pixmap, .. } => *pixmap,
            GrabSource::WindowDirect { window } => *window,
        }
    }
}

/// Set up XComposite for a window: name its off-screen pixmap, redirecting it
/// ourselves only if no one else has. Degrades to a direct grab on any failure.
fn setup_window_grab(conn: &RustConnection, window: xproto::Window) -> GrabSource {
    let have_composite = conn
        .composite_query_version(0, 4)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some();
    if !have_composite {
        eprintln!("[x11] XComposite unavailable; capturing the window's visible region only");
        return GrabSource::WindowDirect { window };
    }

    // A running compositor already redirects subwindows, so try naming the
    // pixmap first and only redirect ourselves if that fails.
    if let Ok(pixmap) = name_window_pixmap(conn, window) {
        return GrabSource::Window {
            window,
            pixmap,
            redirected_by_us: false,
        };
    }

    let redirected = conn
        .composite_redirect_window(window, Redirect::AUTOMATIC)
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !redirected {
        eprintln!("[x11] composite redirect failed; capturing the visible region only");
        return GrabSource::WindowDirect { window };
    }

    match name_window_pixmap(conn, window) {
        Ok(pixmap) => GrabSource::Window {
            window,
            pixmap,
            redirected_by_us: true,
        },
        Err(e) => {
            eprintln!("[x11] NameWindowPixmap failed after redirect ({e}); visible region only");
            let _ = conn.composite_unredirect_window(window, Redirect::AUTOMATIC);
            GrabSource::WindowDirect { window }
        }
    }
}

/// Allocate a pixmap id and bind it to the window's current contents. The id is
/// invalidated when the window resizes, so callers re-name on grab failure.
fn name_window_pixmap(
    conn: &RustConnection,
    window: xproto::Window,
) -> Result<xproto::Pixmap, CaptureError> {
    let pixmap = conn.generate_id()?;
    conn.composite_name_window_pixmap(window, pixmap)?
        .check()
        .map_err(|e| CaptureError::Backend(format!("NameWindowPixmap: {e}")))?;
    Ok(pixmap)
}

/// Release the composite pixmap and undo any redirect we installed.
fn teardown_window_grab(conn: &RustConnection, source: &GrabSource) {
    if let GrabSource::Window {
        window,
        pixmap,
        redirected_by_us,
    } = source
    {
        let _ = conn.free_pixmap(*pixmap);
        if *redirected_by_us {
            let _ = conn.composite_unredirect_window(*window, Redirect::AUTOMATIC);
        }
        let _ = conn.flush();
    }
}

/// Whether the MIT-SHM fast path is usable: the code must be compiled in (libc
/// present) *and* the running server must advertise the extension.
#[cfg(feature = "capture-x11")]
fn shm_available(conn: &RustConnection) -> bool {
    use x11rb::protocol::shm::ConnectionExt as _;
    // NOTE: `shm_query_version` is the canonical presence check; it errors if the
    // extension is missing on this server.
    match conn.shm_query_version() {
        Ok(cookie) => cookie.reply().is_ok(),
        Err(_) => false,
    }
}

/// SHM path not compiled (no libc under a pure `capture-x11` build).
#[cfg(not(feature = "capture-x11"))]
fn shm_available(_conn: &RustConnection) -> bool {
    false
}

/// Inspect the visual referenced by `visual_id` to choose a [`PixelFormat`].
fn detect_format(setup: &xproto::Setup, visual_id: xproto::Visualid) -> PixelFormat {
    let visual = setup
        .roots
        .iter()
        .flat_map(|s| s.allowed_depths.iter())
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == visual_id);

    let big_endian = matches!(setup.image_byte_order, xproto::ImageOrder::MSB_FIRST);

    if let Some(v) = visual {
        // Standard little-endian TrueColor: R=0xff0000 G=0xff00 B=0xff.
        // Ascending memory bytes are B,G,R,(pad) -> Bgrx8888.
        let standard = v.red_mask == 0x00ff_0000
            && v.green_mask == 0x0000_ff00
            && v.blue_mask == 0x0000_00ff;
        if standard && !big_endian {
            return PixelFormat::Bgrx8888;
        }
        // Same masks, MSB-first server: ascending bytes X,R,G,B -> Xrgb8888.
        if standard && big_endian {
            return PixelFormat::Xrgb8888;
        }
        // Swapped masks R=0xff G=0xff00 B=0xff0000, little-endian -> Rgbx8888.
        let swapped = v.red_mask == 0x0000_00ff
            && v.green_mask == 0x0000_ff00
            && v.blue_mask == 0x00ff_0000;
        if swapped && !big_endian {
            return PixelFormat::Rgbx8888;
        }
    }

    // NOTE: Unknown visual / exotic byte order. Bgrx8888 is correct for the vast
    // majority of real X11 desktops. Needs a Linux box with the actual server to
    // confirm the mask/endianness combination in the wild.
    PixelFormat::Bgrx8888
}

/// X11 frame source (root or single window).
struct X11Source {
    name: &'static str,
    /// Connection held only until `start()` hands it to the capture thread.
    conn: Option<Arc<RustConnection>>,
    root: xproto::Window,
    mode: TargetMode,
    /// Depth of the grabbed drawable (root or window).
    depth: u8,
    has_shm: bool,
    info: Arc<Mutex<StreamInfo>>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FrameSource for X11Source {
    fn name(&self) -> &str {
        self.name
    }

    fn stream_info(&self) -> Option<StreamInfo> {
        self.info.lock().ok().map(|g| *g)
    }

    fn start(&mut self, sink: FrameSink) -> Result<(), CaptureError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(CaptureError::Backend("capture already running".into()));
        }

        let conn = self
            .conn
            .take()
            .ok_or_else(|| CaptureError::Backend("x11 source already consumed".into()))?;

        let info = *self
            .info
            .lock()
            .map_err(|_| CaptureError::Backend("stream_info mutex poisoned".into()))?;

        // Install the XComposite redirect / named pixmap here (not in the spawned
        // thread) so failures degrade to a direct grab before we report success.
        let source = match self.mode {
            TargetMode::Root => GrabSource::Root { root: self.root },
            TargetMode::Window(win) => setup_window_grab(&conn, win),
        };

        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let depth = self.depth;
        let has_shm = self.has_shm;

        let handle = std::thread::Builder::new()
            .name("x11-capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(&conn, source, depth, has_shm, info, &running, sink) {
                    // NOTE: the FrameSource contract has no error channel; log and
                    // stop. A production build would surface this to the caller.
                    eprintln!("[x11] capture thread stopped: {e}");
                    running.store(false, Ordering::SeqCst);
                }
            })
            .map_err(|e| CaptureError::Backend(format!("failed to spawn capture thread: {e}")))?;

        self.thread = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for X11Source {
    fn drop(&mut self) {
        self.stop();
        // If start() was never called, undo any redirect/pixmap set up in open().
        // (Currently the redirect is installed in start(), so there is nothing to
        // clean here, but keep the connection alive until drop to be safe.)
        self.conn.take();
    }
}

/// The capture thread body: pace to `framerate`, grab, deliver, repeat, then
/// tear down the SHM segment and any composite redirect.
fn capture_loop(
    conn: &RustConnection,
    mut source: GrabSource,
    depth: u8,
    has_shm: bool,
    info: StreamInfo,
    running: &Arc<AtomicBool>,
    mut sink: FrameSink,
) -> Result<(), CaptureError> {
    debug_assert!(
        depth == 24 || depth == 32,
        "x11 capture assumes a 24/32-bit visual so a ZPixmap row is width*4"
    );

    let width = info.width;
    let height = info.height;
    let stride = width * BPP;
    let frame_bytes = (stride as usize) * (height as usize);

    // Monotonic time base for pts. Instant is guaranteed non-decreasing.
    let start = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / info.framerate.max(1) as f64);

    // Set up the SHM grabber once if available; on any setup failure fall back.
    #[cfg(feature = "capture-x11")]
    let mut shm = if has_shm {
        match shm_impl::ShmGrabber::new(conn, frame_bytes) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("[x11] SHM setup failed ({e}); using get_image");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "capture-x11"))]
    let _ = has_shm;

    let mut outcome = Ok(());

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        let drawable = source.drawable();
        let grab = grab_frame(conn, &mut shm, drawable, width, height, frame_bytes);

        let mut buf = match grab {
            Ok(buf) => buf,
            Err(e) => {
                // For a window, the named pixmap may be stale (the window resized,
                // invalidating it). Re-name it and retry once; if the retry also
                // fails (e.g. the window shrank below the captured size) skip this
                // frame and keep going. Root/direct grabs treat errors as fatal.
                match &mut source {
                    GrabSource::Window { window, pixmap, .. } => {
                        let _ = conn.free_pixmap(*pixmap);
                        match name_window_pixmap(conn, *window) {
                            Ok(fresh) => *pixmap = fresh,
                            Err(_) => {
                                // Window is gone (unmapped/destroyed): stop cleanly.
                                outcome = Err(e);
                                break;
                            }
                        }
                        match grab_frame(conn, &mut shm, *pixmap, width, height, frame_bytes) {
                            Ok(buf) => buf,
                            Err(_) => {
                                // Likely resized smaller; wait for it to grow back.
                                sleep_pacing(frame_interval, tick, running);
                                continue;
                            }
                        }
                    }
                    _ => {
                        outcome = Err(e);
                        break;
                    }
                }
            }
        };

        // Normalize to exactly frame_bytes (the drawable may have resized).
        if buf.len() < frame_bytes {
            buf.resize(frame_bytes, 0);
        } else if buf.len() > frame_bytes {
            buf.truncate(frame_bytes);
        }

        let pts_ns = start.elapsed().as_nanos() as u64;
        sink(Frame {
            width,
            height,
            stride,
            format: info.format,
            pts_ns,
            data: Arc::new(buf),
        });

        sleep_pacing(frame_interval, tick, running);
    }

    // Detach the SHM segment on the server side while the connection is still
    // alive, then let the grabber's Drop unmap the local mapping.
    #[cfg(feature = "capture-x11")]
    if let Some(mut g) = shm.take() {
        g.detach(conn);
    }

    // Release the composite pixmap and undo any redirect we installed.
    teardown_window_grab(conn, &source);

    outcome
}

/// Sleep the remainder of the frame interval in small slices so `stop()` stays
/// responsive.
fn sleep_pacing(frame_interval: Duration, tick: Instant, running: &Arc<AtomicBool>) {
    if let Some(remaining) = frame_interval.checked_sub(tick.elapsed()) {
        let slice = Duration::from_millis(5);
        let mut left = remaining;
        while left > Duration::ZERO && running.load(Ordering::SeqCst) {
            let s = left.min(slice);
            std::thread::sleep(s);
            left = left.saturating_sub(s);
        }
    }
}

/// Grab one frame from `drawable` via the SHM fast path when available, else the
/// portable `get_image` wire path.
#[cfg(feature = "capture-x11")]
fn grab_frame(
    conn: &RustConnection,
    shm: &mut Option<shm_impl::ShmGrabber>,
    drawable: xproto::Drawable,
    width: u32,
    height: u32,
    frame_bytes: usize,
) -> Result<Vec<u8>, CaptureError> {
    if let Some(g) = shm.as_mut() {
        g.grab(conn, drawable, width, height, frame_bytes)
    } else {
        grab_wire(conn, drawable, width, height)
    }
}

#[cfg(not(feature = "capture-x11"))]
fn grab_frame(
    conn: &RustConnection,
    _shm: &mut Option<()>,
    drawable: xproto::Drawable,
    width: u32,
    height: u32,
    _frame_bytes: usize,
) -> Result<Vec<u8>, CaptureError> {
    grab_wire(conn, drawable, width, height)
}

/// Portable fallback: `get_image` returns the pixels inside the reply.
fn grab_wire(
    conn: &RustConnection,
    drawable: xproto::Drawable,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CaptureError> {
    // plane_mask = !0 selects all bit planes; ZPixmap packs one pixel per word.
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            0,
            0,
            width as u16,
            height as u16,
            !0,
        )?
        .reply()?;
    Ok(reply.data)
}

/// MIT-SHM fast path. `capture-x11` pulls in `dep:libc` for the SysV shm calls,
/// so this is compiled whenever the module is.
#[cfg(feature = "capture-x11")]
mod shm_impl {
    use super::*;
    use x11rb::protocol::shm::{self, ConnectionExt as _};

    /// A reusable shared-memory segment attached to the X server, plus the
    /// server-side [`shm::Seg`] id. One allocation feeds every frame.
    ///
    /// The grabber does NOT own the X connection: the capture loop owns it and
    /// must call [`ShmGrabber::detach`] (passing the connection) before the
    /// grabber is dropped, so the server-side detach happens while the socket is
    /// still alive. `Drop` unmaps the local mapping unconditionally as a backstop.
    pub(super) struct ShmGrabber {
        seg: shm::Seg,
        addr: *mut u8,
        size: usize,
        /// Local mapping already unmapped? Prevents a double `shmdt`.
        unmapped: bool,
    }

    // The raw pointer is only ever touched on the capture thread; asserting Send
    // lets the grabber live in that thread's stack across the loop.
    unsafe impl Send for ShmGrabber {}

    impl ShmGrabber {
        /// Allocate `size` bytes of SysV shared memory and attach to the server.
        ///
        /// NOTE: `x11rb` does not wrap the SysV shm syscalls, so we call libc
        /// `shmget`/`shmat`/`shmctl` directly — the standard x11rb SHM pattern.
        pub(super) fn new(conn: &RustConnection, size: usize) -> Result<Self, CaptureError> {
            // SAFETY: create a private R/W segment, map it, then IPC_RMID it so
            // the kernel frees it once we and the server both detach.
            unsafe {
                const IPC_PRIVATE: i32 = 0;
                const IPC_CREAT: i32 = 0o1000;
                const IPC_RMID: i32 = 0;
                let shmid = libc::shmget(IPC_PRIVATE, size, IPC_CREAT | 0o600);
                if shmid == -1 {
                    return Err(CaptureError::Backend("shmget failed".into()));
                }
                let addr = libc::shmat(shmid, std::ptr::null(), 0);
                if addr == usize::MAX as *mut libc::c_void {
                    libc::shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                    return Err(CaptureError::Backend("shmat failed".into()));
                }

                let seg = match conn.generate_id() {
                    Ok(seg) => seg,
                    Err(e) => {
                        libc::shmdt(addr);
                        libc::shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                        return Err(CaptureError::Backend(format!("generate_id: {e}")));
                    }
                };
                // read_only = false: the server writes captured pixels here.
                if let Err(e) = conn.shm_attach(seg, shmid as u32, false) {
                    libc::shmdt(addr);
                    libc::shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                    return Err(CaptureError::Backend(format!("shm_attach: {e}")));
                }
                let _ = conn.flush();
                // Free-on-last-detach; safe to request immediately after attach.
                libc::shmctl(shmid, IPC_RMID, std::ptr::null_mut());

                Ok(ShmGrabber {
                    seg,
                    addr: addr as *mut u8,
                    size,
                    unmapped: false,
                })
            }
        }

        /// Perform one `shm_get_image` and copy the pixels into an owned Vec.
        pub(super) fn grab(
            &mut self,
            conn: &RustConnection,
            drawable: xproto::Drawable,
            width: u32,
            height: u32,
            frame_bytes: usize,
        ) -> Result<Vec<u8>, CaptureError> {
            // offset = 0, plane_mask = !0, ZPixmap into our segment at offset 0.
            let _reply = conn
                .shm_get_image(
                    drawable,
                    0,
                    0,
                    width as u16,
                    height as u16,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    self.seg,
                    0,
                )?
                .reply()?;
            // SAFETY: the server has finished writing (we awaited the reply); the
            // mapping is `size` bytes and outlives this borrow.
            let src = unsafe { std::slice::from_raw_parts(self.addr, self.size) };
            let n = frame_bytes.min(src.len());
            Ok(src[..n].to_vec())
        }

        /// Detach from the server and unmap locally. Call before drop while the
        /// connection is still alive. Idempotent for the local mapping.
        pub(super) fn detach(&mut self, conn: &RustConnection) {
            let _ = conn.shm_detach(self.seg);
            let _ = conn.flush();
            if !self.unmapped {
                // SAFETY: addr came from shmat and has not been detached yet.
                unsafe {
                    libc::shmdt(self.addr as *mut libc::c_void);
                }
                self.unmapped = true;
            }
        }
    }

    impl Drop for ShmGrabber {
        fn drop(&mut self) {
            if !self.unmapped {
                // Backstop: server-side detach should already have happened via
                // `detach()`. The segment itself was IPC_RMID'd at creation, so
                // once this last local mapping goes the kernel reclaims it.
                // SAFETY: addr came from shmat and is still mapped.
                unsafe {
                    libc::shmdt(self.addr as *mut libc::c_void);
                }
                self.unmapped = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(class: Option<&str>, instance: Option<&str>, title: Option<&str>) -> WindowDescriptor {
        WindowDescriptor {
            class: class.map(str::to_string),
            instance: instance.map(str::to_string),
            title: title.map(str::to_string),
        }
    }

    #[test]
    fn descriptor_round_trips() {
        let d = desc(Some("steam_app_570"), Some("dota2"), Some("Dota 2"));
        assert_eq!(parse_descriptor(&format_descriptor(&d)), Some(d));
    }

    #[test]
    fn descriptor_round_trips_partial() {
        // Only a class (no instance/title) is still enough to re-attach.
        let d = desc(Some("firefox"), None, None);
        assert_eq!(parse_descriptor(&format_descriptor(&d)), Some(d));
    }

    #[test]
    fn parse_ignores_unknown_keys_and_blank_lines() {
        let text = "class=mpv\n\n# a comment line without '='\nfoo=bar\ntitle=video.mkv\n";
        assert_eq!(parse_descriptor(text), Some(desc(Some("mpv"), None, Some("video.mkv"))));
    }

    #[test]
    fn parse_empty_is_none() {
        assert_eq!(parse_descriptor(""), None);
        assert_eq!(parse_descriptor("nonsense without equals\n"), None);
    }

    #[test]
    fn value_may_contain_equals() {
        // Titles routinely contain '='; only the first '=' splits the line.
        let d = parse_descriptor("title=a=b=c\n").unwrap();
        assert_eq!(d.title.as_deref(), Some("a=b=c"));
    }

    #[test]
    fn newlines_in_title_are_folded() {
        let d = desc(Some("term"), None, Some("line1\nline2"));
        // A folded title still round-trips as a single line (spaces for newlines).
        let parsed = parse_descriptor(&format_descriptor(&d)).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("line1 line2"));
    }
}
