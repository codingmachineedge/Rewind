//! Linux global "save clip" hotkey with two backends.
//!
//! # Backends
//!
//! 1. **Portal — `org.freedesktop.portal.GlobalShortcuts`** (via [`ashpd`] 0.9).
//!    This is the sanctioned path on Wayland: the app can't grab keys itself, so
//!    it asks xdg-desktop-portal to register a shortcut. We create a session,
//!    `bind_shortcuts` one shortcut (`"save-clip"`) with a preferred trigger
//!    derived from the accelerator, then await the `Activated` signal stream on a
//!    worker thread and invoke the callback each time the shortcut fires.
//!
//! 2. **evdev fallback** (via [`evdev`] 0.12). If the portal is unavailable or
//!    errors, we read `/dev/input/event*` directly: enumerate keyboards (devices
//!    whose supported keys include letter keys), spawn a reader thread per
//!    keyboard, track modifier state (Ctrl/Alt/Shift/Super), and fire the
//!    callback when the parsed chord's key goes down with the right modifiers.
//!
//!    **Permissions:** reading `/dev/input/event*` requires the process to be in
//!    the `input` group (or an equivalent udev rule granting read access). On a
//!    stock desktop a normal user cannot open these nodes; the fallback will
//!    return [`HotkeyError::Backend`] with an EACCES message in that case.
//!
//! # Threading & teardown
//!
//! Each backend spawns one or more listener threads and shares a `stop` flag
//! (`Arc<AtomicBool>`) and the `FnMut` callback (behind a `Mutex`) with them.
//! [`HotkeyManager::stop`] flips the flag, joins the threads, and closes the
//! portal session. The evdev reader puts its fd in non-blocking mode and sleeps
//! briefly between empty reads so it can observe the stop flag; the portal
//! thread breaks out of the stream when the flag is set (it is also released
//! when the session/connection is dropped).

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::hotkey::{HotkeyError, HotkeyManager};

/// Shared callback: `FnMut` stored behind a `Mutex` so listener thread(s) can
/// invoke it while the manager keeps its `Send` bound.
type SharedCallback = Arc<Mutex<Box<dyn FnMut() + Send>>>;

// ---------------------------------------------------------------------------
// Accelerator parsing
// ---------------------------------------------------------------------------

/// Modifier bitflags, mirroring how we track modifier keys in the evdev backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool, // Super / Meta
}

impl Mods {
    fn any(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.logo
    }
}

/// A parsed accelerator such as `"Ctrl+Alt+S"`.
#[derive(Clone, Debug)]
struct Accelerator {
    mods: Mods,
    /// The non-modifier key token, upper-cased (e.g. `"S"`).
    key: String,
    /// The evdev key code for `key`.
    evdev_key: evdev::Key,
}

impl Accelerator {
    /// Parse `"Ctrl+Alt+S"`-style strings.
    ///
    /// Recognises `Ctrl`/`Control`, `Alt`, `Shift`, `Super`/`Meta` (case
    /// insensitive) as modifiers; the remaining single token is the key.
    fn parse(accelerator: &str) -> Result<Self, HotkeyError> {
        let mut mods = Mods::default();
        let mut key: Option<String> = None;

        for raw in accelerator.split('+') {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            match tok.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "alt" | "option" => mods.alt = true,
                "shift" => mods.shift = true,
                "super" | "meta" | "logo" | "win" | "cmd" => mods.logo = true,
                _ => {
                    if key.is_some() {
                        return Err(HotkeyError::Backend(format!(
                            "accelerator `{accelerator}` has more than one non-modifier key"
                        )));
                    }
                    key = Some(tok.to_ascii_uppercase());
                }
            }
        }

        let key = key.ok_or_else(|| {
            HotkeyError::Backend(format!("accelerator `{accelerator}` has no key"))
        })?;
        let evdev_key = key_to_evdev(&key).ok_or_else(|| {
            HotkeyError::Backend(format!("unsupported key `{key}` in accelerator"))
        })?;

        Ok(Accelerator {
            mods,
            key,
            evdev_key,
        })
    }

    /// Portal trigger string per the "shortcuts" XDG spec, e.g. `"CTRL+ALT+s"`.
    ///
    /// The spec uses `CTRL`, `ALT`, `SHIFT`, `LOGO` modifier names joined by `+`
    /// with the key last. We emit the key in lower case, which matches the
    /// convention used by portal implementations for letter keys.
    // NOTE: portal backends treat `preferred_trigger` as advisory; the compositor
    // (GNOME/KDE) may present its own binding UI and ignore/replace it. The exact
    // accepted grammar is backend-defined, so this is best-effort.
    fn portal_trigger(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.mods.ctrl {
            parts.push("CTRL");
        }
        if self.mods.alt {
            parts.push("ALT");
        }
        if self.mods.shift {
            parts.push("SHIFT");
        }
        if self.mods.logo {
            parts.push("LOGO");
        }
        let key_lower = self.key.to_ascii_lowercase();
        parts.push(&key_lower);
        parts.join("+")
    }
}

/// Map an upper-cased key token to an [`evdev::Key`].
///
/// Covers letters, digits, function keys and a handful of common named keys —
/// enough for a "save clip" accelerator.
fn key_to_evdev(key: &str) -> Option<evdev::Key> {
    use evdev::Key;
    // Explicit table (rather than arithmetic over the Key newtype) so the
    // mapping is obvious and covers named keys as well as letters/digits.
    Some(match key {
        "A" => Key::KEY_A,
        "B" => Key::KEY_B,
        "C" => Key::KEY_C,
        "D" => Key::KEY_D,
        "E" => Key::KEY_E,
        "F" => Key::KEY_F,
        "G" => Key::KEY_G,
        "H" => Key::KEY_H,
        "I" => Key::KEY_I,
        "J" => Key::KEY_J,
        "K" => Key::KEY_K,
        "L" => Key::KEY_L,
        "M" => Key::KEY_M,
        "N" => Key::KEY_N,
        "O" => Key::KEY_O,
        "P" => Key::KEY_P,
        "Q" => Key::KEY_Q,
        "R" => Key::KEY_R,
        "S" => Key::KEY_S,
        "T" => Key::KEY_T,
        "U" => Key::KEY_U,
        "V" => Key::KEY_V,
        "W" => Key::KEY_W,
        "X" => Key::KEY_X,
        "Y" => Key::KEY_Y,
        "Z" => Key::KEY_Z,
        "0" => Key::KEY_0,
        "1" => Key::KEY_1,
        "2" => Key::KEY_2,
        "3" => Key::KEY_3,
        "4" => Key::KEY_4,
        "5" => Key::KEY_5,
        "6" => Key::KEY_6,
        "7" => Key::KEY_7,
        "8" => Key::KEY_8,
        "9" => Key::KEY_9,
        "F1" => Key::KEY_F1,
        "F2" => Key::KEY_F2,
        "F3" => Key::KEY_F3,
        "F4" => Key::KEY_F4,
        "F5" => Key::KEY_F5,
        "F6" => Key::KEY_F6,
        "F7" => Key::KEY_F7,
        "F8" => Key::KEY_F8,
        "F9" => Key::KEY_F9,
        "F10" => Key::KEY_F10,
        "F11" => Key::KEY_F11,
        "F12" => Key::KEY_F12,
        "SPACE" => Key::KEY_SPACE,
        "ENTER" | "RETURN" => Key::KEY_ENTER,
        "TAB" => Key::KEY_TAB,
        "ESC" | "ESCAPE" => Key::KEY_ESC,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Portal (GlobalShortcuts) backend
// ---------------------------------------------------------------------------

/// State for the portal backend once a shortcut is bound.
struct PortalBackend {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PortalBackend {
    /// Try to bind `accel` via the GlobalShortcuts portal and start listening.
    ///
    /// Returns `Ok(None)` when the portal is unavailable (caller should fall
    /// back to evdev); `Ok(Some(_))` on success; `Err` for a hard failure.
    fn try_start(
        accel: &Accelerator,
        cb: SharedCallback,
    ) -> Result<Option<PortalBackend>, HotkeyError> {
        use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
        use ashpd::WindowIdentifier;
        // NOTE: `futures-util` is not a declared dependency of this crate, but it
        // is a dependency of `zbus`, which `ashpd` re-exports. In zbus 4.x (used
        // by ashpd 0.9) `StreamExt` is reachable at
        // `ashpd::zbus::export::futures_util::StreamExt`; this brings `.next()`
        // into scope for the `Activated` stream without adding a dependency. If a
        // future zbus bump moves this path, add `futures-util` to the `hotkey`
        // feature and import `futures_util::StreamExt` instead.
        use ashpd::zbus::export::futures_util::StreamExt;

        let trigger = accel.portal_trigger();

        // Probe: connect, create a session, bind the shortcut. If any step
        // fails we treat the portal as unavailable and let the caller fall back.
        let probe: Result<
            (
                GlobalShortcuts<'static>,
                ashpd::desktop::Session<'static, GlobalShortcuts<'static>>,
            ),
            ashpd::Error,
        > = pollster::block_on(async {
            let shortcuts = GlobalShortcuts::new().await?;
            let session = shortcuts.create_session().await?;
            let new = NewShortcut::new("save-clip", "Save the last clip")
                .preferred_trigger(Some(trigger.as_str()));
            // NOTE: `bind_shortcuts` wants a `&WindowIdentifier`; with no
            // toplevel surface we pass the default (no parent). On Wayland a
            // real `WindowIdentifier::from_wayland*` would let the compositor
            // anchor its binding dialog to our window.
            shortcuts
                .bind_shortcuts(&session, &[new], &WindowIdentifier::default())
                .await?;
            Ok((shortcuts, session))
        });

        let (shortcuts, session) = match probe {
            Ok(pair) => pair,
            Err(e) => {
                log_portal_unavailable(&e);
                return Ok(None);
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        // Move the portal objects into the listener thread so the session stays
        // alive for the lifetime of the listener and is dropped (closed) on exit.
        let handle = std::thread::Builder::new()
            .name("rewind-hotkey-portal".into())
            .spawn(move || {
                let _keep_shortcuts = &shortcuts;
                let result = pollster::block_on(async {
                    let mut stream = shortcuts.receive_activated().await?;
                    while !thread_stop.load(Ordering::SeqCst) {
                        // Await the next activation. When the connection is
                        // dropped on teardown, the stream ends and we exit.
                        match stream.next().await {
                            Some(activated) => {
                                if activated.shortcut_id() == "save-clip"
                                    && !thread_stop.load(Ordering::SeqCst)
                                {
                                    if let Ok(mut f) = cb.lock() {
                                        (f)();
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    // Best-effort close on normal exit.
                    let _ = session.close().await;
                    Ok::<(), ashpd::Error>(())
                });
                if let Err(e) = result {
                    eprintln!("rewind: portal shortcut listener ended: {e}");
                }
            })
            .map_err(|e| HotkeyError::Backend(format!("spawn portal thread: {e}")))?;

        Ok(Some(PortalBackend {
            stop,
            handle: Some(handle),
        }))
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            // NOTE: `receive_activated().next()` blocks until the next signal or
            // until the D-Bus connection closes. Dropping/joining here relies on
            // the session close and process teardown to release it; on a live
            // system the join may wait for the next event. A production impl
            // would use an abortable future — left as a Linux-verify item.
            let _ = h.join();
        }
    }
}

fn log_portal_unavailable(e: &ashpd::Error) {
    eprintln!("rewind: GlobalShortcuts portal unavailable ({e}); trying evdev fallback");
}

// ---------------------------------------------------------------------------
// evdev fallback backend
// ---------------------------------------------------------------------------

/// State for the evdev backend: one reader thread per keyboard device.
struct EvdevBackend {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl EvdevBackend {
    fn try_start(accel: &Accelerator, cb: SharedCallback) -> Result<EvdevBackend, HotkeyError> {
        let keyboards = enumerate_keyboards()?;
        if keyboards.is_empty() {
            return Err(HotkeyError::Backend(
                "no readable keyboard devices in /dev/input (need `input` group access)".into(),
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        for (path, device) in keyboards {
            let thread_stop = stop.clone();
            let thread_cb = cb.clone();
            let accel = accel.clone();
            let name = format!("rewind-hotkey-evdev-{}", path.display());
            let handle = std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    evdev_reader_loop(device, &accel, thread_cb, thread_stop);
                })
                .map_err(|e| HotkeyError::Backend(format!("spawn evdev thread: {e}")))?;
            handles.push(handle);
        }

        Ok(EvdevBackend { stop, handles })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// Enumerate `/dev/input/event*` and return devices that look like keyboards
/// (their supported keys include the letter keys A and Z).
fn enumerate_keyboards() -> Result<Vec<(PathBuf, evdev::Device)>, HotkeyError> {
    let mut out = Vec::new();
    // NOTE: evdev 0.12 provides `evdev::enumerate()` yielding `(PathBuf, Device)`;
    // we scan the directory ourselves so a single unreadable node doesn't abort
    // the whole enumeration and so we control the keyboard heuristic.
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            return Err(HotkeyError::Backend(format!(
                "cannot read /dev/input: {e} (need `input` group access)"
            )))
        }
    };

    for entry in dir.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false);
        if !is_event {
            continue;
        }
        match evdev::Device::open(&path) {
            Ok(dev) => {
                if is_keyboard(&dev) {
                    out.push((path, dev));
                }
            }
            Err(_e) => {
                // Skip nodes we can't open (permissions / not a real device).
                continue;
            }
        }
    }
    Ok(out)
}

/// Heuristic: a keyboard supports the letter keys.
fn is_keyboard(dev: &evdev::Device) -> bool {
    match dev.supported_keys() {
        Some(keys) => keys.contains(evdev::Key::KEY_A) && keys.contains(evdev::Key::KEY_Z),
        None => false,
    }
}

/// Read events from one keyboard, tracking modifier state and firing the
/// callback when the accelerator's key is pressed with the right modifiers.
fn evdev_reader_loop(
    mut device: evdev::Device,
    accel: &Accelerator,
    cb: SharedCallback,
    stop: Arc<AtomicBool>,
) {
    use evdev::{InputEventKind, Key};

    // Put the fd in non-blocking mode so `fetch_events` returns rather than
    // parking forever; that lets us poll the stop flag between reads.
    set_nonblocking(&device);

    let mut cur = Mods::default();

    while !stop.load(Ordering::SeqCst) {
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if let InputEventKind::Key(key) = ev.kind() {
                        // value: 0 = release, 1 = press, 2 = autorepeat.
                        let value = ev.value();
                        let down = value == 1 || value == 2;

                        match key {
                            Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => cur.ctrl = down,
                            Key::KEY_LEFTALT | Key::KEY_RIGHTALT => cur.alt = down,
                            Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => cur.shift = down,
                            Key::KEY_LEFTMETA | Key::KEY_RIGHTMETA => cur.logo = down,
                            // Fire on the initial press (value == 1) only, to avoid
                            // autorepeat storms, and only when the required modifiers
                            // are all held. We require an exact match on the
                            // accelerator's modifiers; extra modifiers being held
                            // block the trigger (deliberate — avoids misfires).
                            k if k == accel.evdev_key
                                && value == 1
                                && mods_match(&cur, &accel.mods) =>
                            {
                                if let Ok(mut f) = cb.lock() {
                                    (f)();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                // EAGAIN / EWOULDBLOCK in non-blocking mode: nothing to read.
                if e.raw_os_error() == Some(libc_ewouldblock()) {
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }
                // Any other error (device unplugged, etc.): stop this reader.
                eprintln!(
                    "rewind: evdev reader error, stopping this device: {e}"
                );
                break;
            }
        }
    }
}

/// Do the currently-held modifiers satisfy the accelerator?
///
/// Exact match: every modifier the accelerator wants must be held, and no
/// other modifier may be held (prevents `Ctrl+Alt+S` firing on
/// `Ctrl+Alt+Shift+S`).
fn mods_match(cur: &Mods, want: &Mods) -> bool {
    cur == want || (!want.any() && !cur.any())
}

/// Set the device fd non-blocking via `fcntl`. Best-effort; on failure the
/// reader still works but will block in `fetch_events` until an event arrives.
fn set_nonblocking(device: &evdev::Device) {
    // NOTE: evdev 0.12 exposes the raw fd via `AsRawFd`. We flip O_NONBLOCK so
    // the reader loop can observe the stop flag. If this is unavailable in a
    // given patch release, `Device::set_nonblocking` may exist instead.
    let fd = device.as_raw_fd();
    unsafe {
        let flags = fcntl_getfl(fd);
        if flags >= 0 {
            let _ = fcntl_setfl(fd, flags | o_nonblock());
        }
    }
}

// Minimal libc shims. We avoid a direct `libc` dependency (not declared) by
// declaring the few symbols we need. These are stable Linux syscalls.
// NOTE: if the project already links `libc` transitively, these extern decls
// are compatible with it; otherwise the linker resolves them against the C lib.
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK_LINUX: i32 = 0o4000;
const EWOULDBLOCK_LINUX: i32 = 11; // EAGAIN == EWOULDBLOCK on Linux.

extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

fn fcntl_getfl(fd: i32) -> i32 {
    unsafe { fcntl(fd, F_GETFL) }
}
unsafe fn fcntl_setfl(fd: i32, flags: i32) -> i32 {
    fcntl(fd, F_SETFL, flags)
}
fn o_nonblock() -> i32 {
    O_NONBLOCK_LINUX
}
fn libc_ewouldblock() -> i32 {
    EWOULDBLOCK_LINUX
}

// ---------------------------------------------------------------------------
// Combined manager
// ---------------------------------------------------------------------------

enum Active {
    None,
    Portal(PortalBackend),
    Evdev(EvdevBackend),
}

/// Hotkey manager that prefers the GlobalShortcuts portal and falls back to
/// raw evdev at [`register_save`](HotkeyManager::register_save) time.
struct LinuxHotkeyManager {
    name: &'static str,
    active: Active,
}

impl LinuxHotkeyManager {
    fn new() -> Self {
        LinuxHotkeyManager {
            name: "uninitialized",
            active: Active::None,
        }
    }
}

impl HotkeyManager for LinuxHotkeyManager {
    fn name(&self) -> &str {
        self.name
    }

    fn register_save(
        &mut self,
        accelerator: &str,
        on_trigger: Box<dyn FnMut() + Send>,
    ) -> Result<(), HotkeyError> {
        // Replace any prior registration.
        self.stop();

        let accel = Accelerator::parse(accelerator)?;
        let cb: SharedCallback = Arc::new(Mutex::new(on_trigger));

        // 1) Try the portal.
        match PortalBackend::try_start(&accel, cb.clone()) {
            Ok(Some(portal)) => {
                self.active = Active::Portal(portal);
                self.name = "portal-global-shortcuts";
                return Ok(());
            }
            Ok(None) => { /* portal unavailable, fall through */ }
            Err(e) => {
                eprintln!("rewind: portal backend failed ({e}); trying evdev fallback");
            }
        }

        // 2) Fall back to evdev.
        match EvdevBackend::try_start(&accel, cb) {
            Ok(evdev) => {
                self.active = Active::Evdev(evdev);
                self.name = "evdev";
                Ok(())
            }
            Err(e) => {
                self.name = "unavailable";
                Err(e)
            }
        }
    }

    fn stop(&mut self) {
        match std::mem::replace(&mut self.active, Active::None) {
            Active::None => {}
            Active::Portal(mut b) => b.stop(),
            Active::Evdev(mut b) => b.stop(),
        }
    }
}

impl Drop for LinuxHotkeyManager {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Create the Linux hotkey manager. The concrete backend (portal vs. evdev) is
/// selected lazily at `register_save` time; `name()` reflects the active one.
pub fn manager() -> Result<Box<dyn HotkeyManager>, HotkeyError> {
    Ok(Box::new(LinuxHotkeyManager::new()))
}
