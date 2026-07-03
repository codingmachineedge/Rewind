//! X11 screen-capture backend using `x11rb`, with an MIT-SHM (XShm) fast path.
//!
//! This backend captures the **root window** of the default screen at a fixed
//! framerate and delivers each grab to the [`FrameSink`] as a [`Frame`].
//!
//! Two capture paths exist:
//!
//! * **XShm** (fast, zero socket copy): the server writes the captured image
//!   straight into a POSIX shared-memory segment this process maps. Attaching a
//!   SysV shm segment requires the libc `shmget`/`shmat`/`shmctl`/`shmdt` calls,
//!   which `x11rb` intentionally does not wrap. Because the `libc` crate is only
//!   pulled in by the `capture-wayland` feature in this workspace (it is not a
//!   dependency of `capture-x11` alone), the SHM path is compiled **only when
//!   `libc` is also available** (`#[cfg(feature = "capture-wayland")]`). See the
//!   NOTE below — enabling SHM for a pure X11 build is a one-line Cargo change.
//! * **`xproto::get_image`** (portable fallback): pixels travel back inside the
//!   reply over the X socket. Always compiled; correct everywhere, just slower.
//!
//! Both paths request a 32-bit ZPixmap of the root window.
//!
//! Pixel format: on the overwhelmingly common little-endian X server with a
//! 24/32-bit TrueColor visual, a 32-bit ZPixmap image is laid out in memory as
//! `B, G, R, X` per pixel → [`PixelFormat::Bgrx8888`]. We inspect the root
//! visual's RGB masks and the server byte order to pick the format, defaulting to
//! Bgrx on anything unusual.
//!
//! ## Not yet handled (TODO)
//! - **Per-window / region capture.** Only the whole root window is grabbed.
//!   Real game-clip capture wants a specific window via `composite` redirect
//!   (`RedirectAutomatic`) + `NameWindowPixmap` so occluded/off-screen windows
//!   still render. The `composite` feature is enabled for this.
//! - **Hardware cursor compositing.** `get_image` omits the cursor sprite; it
//!   needs `xfixes::get_cursor_image` + manual alpha-blend.
//! - **RandR multi-monitor.** We capture the full virtual screen; cropping to one
//!   output needs `randr::get_monitors`.
//! - **Geometry changes.** Root size is sampled once at `start()`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::capture::{CaptureError, FrameSink, FrameSource};
use crate::media::{Frame, PixelFormat, StreamInfo};

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

/// Open the X11 capture backend.
///
/// Connects to the X server named by `$DISPLAY`, selects the default screen and
/// its root window, samples the geometry, chooses a pixel format, and (when the
/// SHM path is compiled in) probes for the MIT-SHM extension. The returned source
/// is *not* running yet — call [`FrameSource::start`].
pub fn open() -> Result<Box<dyn FrameSource>, CaptureError> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| CaptureError::Unsupported(format!("cannot connect to X server: {e}")))?;

    let setup = conn.setup();
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or_else(|| CaptureError::Backend(format!("no screen at index {screen_num}")))?;
    let root = screen.root;
    let root_depth = screen.root_depth;
    let root_visual = screen.root_visual;

    // Sample root geometry now so callers can read stream_info() before start().
    let geom = conn
        .get_geometry(root)?
        .reply()
        .map_err(|e| CaptureError::Backend(format!("get_geometry(root) failed: {e}")))?;
    let width = geom.width as u32;
    let height = geom.height as u32;

    if width == 0 || height == 0 {
        return Err(CaptureError::Backend(
            "root window reports zero size".into(),
        ));
    }

    let format = detect_format(setup, root_visual);

    // Probe SHM only if the SHM path is even compiled in (needs libc).
    let has_shm = shm_available(&conn);

    let info = StreamInfo {
        width,
        height,
        framerate: DEFAULT_FRAMERATE,
        format,
    };

    let name = if has_shm { "x11-xshm" } else { "x11-getimage" };

    Ok(Box::new(X11Source {
        name,
        conn: Some(Arc::new(conn)),
        root,
        root_depth,
        has_shm,
        info: Arc::new(Mutex::new(info)),
        running: Arc::new(AtomicBool::new(false)),
        thread: None,
    }))
}

/// Whether the MIT-SHM fast path is usable: the code must be compiled in (libc
/// present) *and* the running server must advertise the extension.
#[cfg(feature = "capture-wayland")]
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
#[cfg(not(feature = "capture-wayland"))]
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

/// X11 root-window frame source.
struct X11Source {
    name: &'static str,
    /// Connection held only until `start()` hands it to the capture thread.
    conn: Option<Arc<RustConnection>>,
    root: xproto::Window,
    root_depth: u8,
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

        self.running.store(true, Ordering::SeqCst);

        let running = Arc::clone(&self.running);
        let root = self.root;
        let root_depth = self.root_depth;
        let has_shm = self.has_shm;

        let handle = std::thread::Builder::new()
            .name("x11-capture".into())
            .spawn(move || {
                if let Err(e) =
                    capture_loop(&conn, root, root_depth, has_shm, info, &running, sink)
                {
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
    }
}

/// The capture thread body: pace to `framerate`, grab, deliver, repeat.
fn capture_loop(
    conn: &RustConnection,
    root: xproto::Window,
    root_depth: u8,
    has_shm: bool,
    info: StreamInfo,
    running: &Arc<AtomicBool>,
    mut sink: FrameSink,
) -> Result<(), CaptureError> {
    debug_assert!(
        root_depth == 24 || root_depth == 32,
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
    #[cfg(feature = "capture-wayland")]
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
    #[cfg(not(feature = "capture-wayland"))]
    let _ = has_shm;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Grab into an owned Vec<u8> in ZPixmap order.
        let mut buf: Vec<u8>;

        #[cfg(feature = "capture-wayland")]
        {
            if let Some(g) = shm.as_mut() {
                buf = g.grab(conn, root, width, height, frame_bytes)?;
            } else {
                buf = grab_wire(conn, root, width, height)?;
            }
        }
        #[cfg(not(feature = "capture-wayland"))]
        {
            buf = grab_wire(conn, root, width, height)?;
        }

        // Normalize to exactly frame_bytes (root may have resized under us).
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

        // Pace to target fps; sleep in slices so stop() stays responsive.
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

    // Detach the SHM segment on the server side while the connection is still
    // alive, then let the grabber's Drop unmap the local mapping.
    #[cfg(feature = "capture-wayland")]
    if let Some(mut g) = shm.take() {
        g.detach(conn);
    }

    Ok(())
}

/// Portable fallback: `get_image` returns the pixels inside the reply.
fn grab_wire(
    conn: &RustConnection,
    root: xproto::Window,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CaptureError> {
    // plane_mask = !0 selects all bit planes; ZPixmap packs one pixel per word.
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            0,
            0,
            width as u16,
            height as u16,
            !0,
        )?
        .reply()?;
    Ok(reply.data)
}

/// MIT-SHM fast path. Compiled only when `libc` is available in the build (see
/// the module docs); guarded behind the `capture-wayland` feature which is the
/// crate's sole source of `dep:libc`.
#[cfg(feature = "capture-wayland")]
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
            root: xproto::Window,
            width: u32,
            height: u32,
            frame_bytes: usize,
        ) -> Result<Vec<u8>, CaptureError> {
            // offset = 0, plane_mask = !0, ZPixmap into our segment at offset 0.
            let _reply = conn
                .shm_get_image(
                    root,
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
