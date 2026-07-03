//! Wayland screen capture via xdg-desktop-portal ScreenCast + PipeWire.
//!
//! On Wayland there is no direct screen-grab API; a compositor-mediated grant is
//! required. We drive [`ashpd`]'s `ScreenCast` portal to obtain a PipeWire node
//! id plus a shared PipeWire remote fd, then open a [`pipewire::stream::Stream`]
//! against that node and pull raw video frames on a dedicated thread running the
//! PipeWire main loop.
//!
//! Flow:
//! 1. `Screencast::new()` → `create_session` → `select_sources(Monitor,
//!    embedded cursor, persistent grant, optional restore token)` → `start`.
//!    `start` yields the negotiated streams (each carries a PipeWire node id) and
//!    an updated restore token; `open_pipe_wire_remote` yields the fd.
//! 2. The portal grant is remembered across launches via a `restore_token`
//!    persisted to `~/.config/rewind/screencast.token` (honoring
//!    `$XDG_CONFIG_HOME`). Loading it and passing it to `select_sources` avoids
//!    re-prompting the user on subsequent runs.
//! 3. A background thread runs the PipeWire `MainLoop`. The stream `process`
//!    callback dequeues a buffer, reads the negotiated SPA video format/size,
//!    copies the CPU-mapped pixels into a `Vec<u8>`, and hands a [`Frame`] to the
//!    sink with a `CLOCK_MONOTONIC` timestamp.
//!
//! All async portal calls are driven from sync code with [`pollster::block_on`].
//!
//! ## Limitations / TODO
//! - DMABUF buffers (`spa_sys::SPA_DATA_DmaBuf`) are not yet mapped; only
//!   `MemPtr` / `MemFd` CPU buffers are copied. A GPU-import path (import the
//!   dmabuf, download to CPU, or forward the fd to the encoder) is left as a
//!   follow-up. When a dmabuf-only negotiation happens the frame is skipped.
//! - We request only the first monitor stream; multi-monitor selection is not
//!   surfaced yet.

use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use ashpd::WindowIdentifier;

use crate::capture::{CaptureError, FrameSink, FrameSource};
use crate::media::{Frame, PixelFormat, StreamInfo};

const BACKEND_NAME: &str = "wayland-pipewire";

/// Public entry point: negotiate a ScreenCast grant and return a ready-to-start
/// [`FrameSource`]. The portal dialog (and thus any user prompt) happens here in
/// [`open`]; [`WaylandSource::start`] only wires up the PipeWire stream.
pub fn open() -> Result<Box<dyn FrameSource>, CaptureError> {
    let grant = pollster::block_on(negotiate_portal())?;
    Ok(Box::new(WaylandSource::new(grant)))
}

/// Everything the portal handed us that PipeWire needs, kept alive for the life
/// of the source. The `OwnedFd` must outlive the stream — dropping it closes the
/// PipeWire remote.
struct PortalGrant {
    /// PipeWire node id of the selected monitor stream.
    node_id: u32,
    /// Shared PipeWire remote fd from `open_pipe_wire_remote`.
    fd: OwnedFd,
}

/// Perform the full portal handshake. Returns [`CaptureError::PermissionDenied`]
/// when the user cancels the dialog.
async fn negotiate_portal() -> Result<PortalGrant, CaptureError> {
    let proxy = Screencast::new()
        .await
        .map_err(map_portal_err)?;

    let session = proxy
        .create_session()
        .await
        .map_err(map_portal_err)?;

    let restore_token = load_restore_token();

    // NOTE: ashpd 0.9 `select_sources` signature is
    //   select_sources(&session, CursorMode, BitFlags<SourceType>, multiple: bool,
    //                   restore_token: Option<&str>, PersistMode) -> Result<Request<()>>
    // The exact argument order/borrowing has shifted between 0.8/0.9 point
    // releases — verify on a Linux build.
    proxy
        .select_sources(
            &session,
            CursorMode::Embedded,
            SourceType::Monitor.into(),
            false, // multiple: single monitor
            restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(map_portal_err)?;

    // `start` shows the picker (unless a valid restore token skipped it) and
    // returns the granted streams plus a fresh restore token.
    let response = proxy
        .start(&session, &WindowIdentifier::default())
        .await
        .map_err(map_portal_err)?
        .response()
        .map_err(map_portal_err)?;

    // Persist the (possibly renewed) restore token so the next launch is silent.
    if let Some(token) = response.restore_token() {
        save_restore_token(token);
    }

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| CaptureError::Backend("portal returned no streams".into()))?;

    let node_id = stream.pipe_wire_node_id();

    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .map_err(map_portal_err)?;

    // NOTE: `open_pipe_wire_remote` returns an `OwnedFd` in ashpd 0.9. If a given
    // point release yields a RawFd instead, wrap it with `OwnedFd::from_raw_fd`.
    Ok(PortalGrant { node_id, fd })
}

/// Map an ashpd portal error to our [`CaptureError`]. The portal signals user
/// cancellation via `ashpd::Error::Response(ResponseError::Cancelled)`.
fn map_portal_err(err: ashpd::Error) -> CaptureError {
    match err {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            CaptureError::PermissionDenied
        }
        // Some portal implementations report a denied/other response for a
        // rejected grant; treat any `Response` error as a permission failure and
        // everything else as a backend error.
        ashpd::Error::Response(_) => CaptureError::PermissionDenied,
        other => CaptureError::Backend(format!("portal: {other}")),
    }
}

// --- restore-token persistence -------------------------------------------------

/// `${XDG_CONFIG_HOME:-~/.config}/rewind/screencast.token`.
fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rewind").join("screencast.token"))
}

fn load_restore_token() -> Option<String> {
    let path = token_path()?;
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

fn save_restore_token(token: &str) {
    let Some(path) = token_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Best-effort: a failure here just means we re-prompt next launch.
    let _ = fs::write(&path, token);
}

// --- FrameSource implementation ------------------------------------------------

/// Shared state for the PipeWire capture thread. `StreamInfo` is negotiated
/// asynchronously (on the first `param_changed`), so it lives behind a mutex.
struct Shared {
    info: Mutex<Option<StreamInfo>>,
}

pub struct WaylandSource {
    grant: Option<PortalGrant>,
    shared: Arc<Shared>,
    /// Set to `false` to ask the PipeWire loop to quit.
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandSource {
    fn new(grant: PortalGrant) -> Self {
        Self {
            grant: Some(grant),
            shared: Arc::new(Shared {
                info: Mutex::new(None),
            }),
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
}

impl FrameSource for WaylandSource {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    fn stream_info(&self) -> Option<StreamInfo> {
        *self.shared.info.lock().unwrap()
    }

    fn start(&mut self, sink: FrameSink) -> Result<(), CaptureError> {
        let grant = self
            .grant
            .take()
            .ok_or_else(|| CaptureError::Backend("capture already started".into()))?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let shared = self.shared.clone();

        // The PipeWire main loop and stream are not `Send`, so they must be
        // created *inside* the thread. Only the raw fd and node id cross the
        // boundary; the `OwnedFd` is moved in and dropped when the thread ends.
        let node_id = grant.node_id;
        let fd = grant.fd;

        let handle = std::thread::Builder::new()
            .name("rewind-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_pipewire_loop(node_id, fd, sink, shared, running) {
                    eprintln!("rewind: pipewire capture thread failed: {e}");
                }
            })
            .map_err(|e| CaptureError::Backend(format!("spawn capture thread: {e}")))?;

        self.thread = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            // The loop polls `running` via a timer and calls `MainLoop::quit`;
            // joining waits for a clean teardown of the stream + loop.
            let _ = handle.join();
        }
    }
}

impl Drop for WaylandSource {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- PipeWire capture thread ---------------------------------------------------

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

/// Per-callback state shared between the stream closures via `Rc<RefCell<..>>`
/// (all closures run on the single loop thread).
struct StreamState {
    shared: Arc<Shared>,
    sink: FrameSink,
    format: spa::param::video::VideoInfoRaw,
    have_format: bool,
    epoch: std::time::Instant,
}

/// Build and run the PipeWire main loop until `running` clears. All PipeWire
/// objects live and die on this thread (they are `!Send`).
fn run_pipewire_loop(
    node_id: u32,
    fd: OwnedFd,
    sink: FrameSink,
    shared: Arc<Shared>,
    running: Arc<AtomicBool>,
) -> Result<(), String> {
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None)
        .map_err(|e| format!("MainLoop::new: {e}"))?;
    let context =
        pw::context::Context::new(&mainloop).map_err(|e| format!("Context::new: {e}"))?;

    // Connect to the PipeWire daemon *through the portal-provided fd* so we see
    // the ScreenCast node. `connect_fd` consumes the fd.
    // NOTE: pipewire-rs 0.8 exposes `Context::connect_fd(fd, properties)`.
    let core = context
        .connect_fd(fd, None)
        .map_err(|e| format!("Context::connect_fd: {e}"))?;

    // Stream properties: identify ourselves as a video-capture consumer.
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };

    let stream = pw::stream::Stream::new(&core, "rewind-screencast", props)
        .map_err(|e| format!("Stream::new: {e}"))?;

    let state = std::rc::Rc::new(std::cell::RefCell::new(StreamState {
        shared: shared.clone(),
        sink,
        format: spa::param::video::VideoInfoRaw::new(),
        have_format: false,
        epoch: std::time::Instant::now(),
    }));

    // Keep the listener alive for the loop's duration.
    let _listener = {
        let state_pc = state.clone();
        let state_proc = state.clone();
        stream
            .add_local_listener_with_user_data(())
            .param_changed(move |_stream, _ud, id, param| {
                on_param_changed(id, param, &state_pc);
            })
            .process(move |stream, _ud| {
                on_process(stream, &state_proc);
            })
            .register()
            .map_err(|e| format!("add_local_listener: {e}"))?
    };

    // Build the "supported formats" POD we offer during negotiation. We accept
    // the common 32-bit packed layouts; PipeWire picks one and reports it back
    // through `param_changed`.
    let format_bytes = build_format_pod_bytes();
    let format_pod = Pod::from_bytes(&format_bytes)
        .ok_or_else(|| "failed to build format POD from bytes".to_string())?;
    let mut pod_pointers: Vec<&Pod> = vec![format_pod];

    // NOTE: pipewire-rs 0.8 `Stream::connect` signature is
    //   connect(direction, target_id: Option<u32>, StreamFlags, &mut [&Pod])
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut pod_pointers,
        )
        .map_err(|e| format!("Stream::connect: {e}"))?;

    // Poll `running` from within the loop: a repeating timer checks the flag and
    // quits the loop when `stop()` clears it.
    let loop_ref = mainloop.loop_();
    let mainloop_quit = mainloop.clone();
    let timer = loop_ref.add_timer(move |_expirations| {
        if !running.load(Ordering::SeqCst) {
            mainloop_quit.quit();
        }
    });
    // Fire every 100 ms starting 100 ms from now.
    // NOTE: `Timer::update_timer` uses `time::Duration`; API is
    //   update_timer(value, interval).update() in pipewire-rs 0.8.
    timer
        .update_timer(
            Some(std::time::Duration::from_millis(100)),
            Some(std::time::Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|e| format!("timer update: {e}"))?;

    mainloop.run();

    // Loop exited: tear down. Dropping `stream`/`core`/`context` closes the fd.
    stream.disconnect().ok();
    Ok(())
}

/// Offer the 32-bit packed RGB formats we can consume, serialized to a POD byte
/// buffer ready for `Stream::connect`. PipeWire chooses one and reports it back
/// through `param_changed`.
fn build_format_pod_bytes() -> Vec<u8> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
            spa::param::video::VideoFormat::xRGB
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 240,
                denom: 1
            }
        ),
    );

    // Serialize the Object POD into raw bytes; `Stream::connect` takes `&Pod`
    // views, which we reconstruct from these bytes at the call site.
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serialize EnumFormat POD")
    .0
    .into_inner()
}

/// Handle the negotiated format: parse the SPA raw-video info and publish a
/// [`StreamInfo`] for `stream_info()`.
fn on_param_changed(
    id: u32,
    param: Option<&Pod>,
    state: &std::rc::Rc<std::cell::RefCell<StreamState>>,
) {
    let Some(param) = param else { return };
    // Only care about the Format param.
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }

    // Confirm this is a raw video format before parsing.
    let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
        Ok(t) => t,
        Err(_) => return,
    };
    if media_type != spa::param::format::MediaType::Video
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return;
    }

    let mut st = state.borrow_mut();
    if st.format.parse(param).is_err() {
        return;
    }
    st.have_format = true;

    let size = st.format.size();
    let framerate = st.format.framerate();
    let pixel = spa_format_to_pixel(st.format.format());

    let info = StreamInfo {
        width: size.width,
        height: size.height,
        framerate: framerate.num.checked_div(framerate.denom).unwrap_or(0),
        format: pixel,
    };

    *st.shared.info.lock().unwrap() = Some(info);
}

/// Dequeue a buffer, copy its CPU pixels, and hand a [`Frame`] to the sink.
fn on_process(
    stream: &pw::stream::StreamRef,
    state: &std::rc::Rc<std::cell::RefCell<StreamState>>,
) {
    // Dequeue the next available buffer; nothing to do if the queue is empty.
    let mut buffer = match stream.dequeue_buffer() {
        Some(b) => b,
        None => return,
    };

    let mut st = state.borrow_mut();
    if !st.have_format {
        return;
    }

    let width = st.format.size().width;
    let height = st.format.size().height;
    let pixel = spa_format_to_pixel(st.format.format());
    let bpp = pixel.bytes_per_pixel() as u32;

    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };

    // Stride comes from the negotiated chunk; fall back to a tight packing.
    let chunk = data.chunk();
    let mut stride = chunk.stride() as u32;
    if stride == 0 {
        stride = width * bpp;
    }
    let chunk_size = chunk.size() as usize;

    // NOTE: DMABUF is not handled — `data.data()` returns `None` for a dmabuf
    // buffer (it isn't CPU-mapped). Skip such frames; see module-level TODO.
    let Some(bytes) = data.data() else {
        return;
    };

    // Copy exactly the payload the producer wrote. Guard against a short/oversized
    // report by clamping to the mapped slice.
    let expected = (stride as usize).saturating_mul(height as usize);
    let copy_len = expected.min(bytes.len()).min(if chunk_size == 0 {
        usize::MAX
    } else {
        chunk_size.max(expected)
    });
    let copy_len = copy_len.min(bytes.len());

    let mut pixels = Vec::with_capacity(copy_len);
    pixels.extend_from_slice(&bytes[..copy_len]);

    let pts_ns = monotonic_now_ns(&st.epoch);

    let frame = Frame {
        width,
        height,
        stride,
        format: pixel,
        pts_ns,
        data: Arc::new(pixels),
    };

    (st.sink)(frame);
}

/// Nanoseconds on a monotonic clock. Prefer `CLOCK_MONOTONIC` via libc; fall back
/// to an `Instant` epoch if the syscall fails.
fn monotonic_now_ns(epoch: &std::time::Instant) -> u64 {
    // SAFETY: `clock_gettime` writes a fully-initialized `timespec`.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) == 0 {
            return (ts.tv_sec as u64)
                .wrapping_mul(1_000_000_000)
                .wrapping_add(ts.tv_nsec as u64);
        }
    }
    epoch.elapsed().as_nanos() as u64
}

/// Map a negotiated SPA video format to our [`PixelFormat`]. Defaults to
/// `Bgrx8888` for anything we don't explicitly recognize.
fn spa_format_to_pixel(fmt: spa::param::video::VideoFormat) -> PixelFormat {
    use spa::param::video::VideoFormat as V;
    match fmt {
        V::BGRx => PixelFormat::Bgrx8888,
        V::RGBx => PixelFormat::Rgbx8888,
        V::BGRA => PixelFormat::Bgra8888,
        V::RGBA => PixelFormat::Rgba8888,
        V::xRGB => PixelFormat::Xrgb8888,
        // NOTE: SPA also has ARGB/ABGR/BGRA_premultiplied etc.; extend as needed.
        _ => PixelFormat::Bgrx8888,
    }
}

// Silence an unused-import lint on hosts where `AsRawFd` isn't otherwise touched;
// it documents that the portal fd is a real OS handle handed to PipeWire.
const _: fn() = || {
    fn _assert_asrawfd<T: AsRawFd>() {}
};
