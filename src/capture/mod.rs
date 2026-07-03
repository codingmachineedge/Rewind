//! Frame capture: a [`FrameSource`] abstraction with Linux backends selected at
//! runtime from `$XDG_SESSION_TYPE`.
//!
//! - Wayland → PipeWire + xdg-desktop-portal ScreenCast (`capture-wayland`).
//! - X11 → XShm / XComposite (`capture-x11`).
//!
//! The trait and selector are dependency-free so the core always compiles; the
//! real backends live behind cargo features.

use std::error::Error;
use std::fmt;

use crate::media::{Frame, StreamInfo};

#[cfg(feature = "capture-wayland")]
pub mod wayland;
#[cfg(feature = "capture-x11")]
pub mod x11;

/// Callback a source invokes for every captured frame, on its own thread.
pub type FrameSink = Box<dyn FnMut(Frame) + Send>;

#[derive(Debug)]
pub enum CaptureError {
    /// The backend isn't available (not built, or wrong session type).
    Unsupported(String),
    /// The user denied the screen-capture permission (portal).
    PermissionDenied,
    /// A backend-specific failure.
    Backend(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Unsupported(s) => write!(f, "capture unsupported: {s}"),
            CaptureError::PermissionDenied => write!(f, "screen capture permission denied"),
            CaptureError::Backend(s) => write!(f, "capture backend error: {s}"),
        }
    }
}

impl Error for CaptureError {}

/// A source of raw frames. Implementations spin up their own capture thread and
/// deliver frames to the sink until [`FrameSource::stop`] is called.
pub trait FrameSource: Send {
    /// Human-readable backend name, e.g. `"wayland-pipewire"`.
    fn name(&self) -> &str;

    /// Negotiated stream info, available once capture has started.
    fn stream_info(&self) -> Option<StreamInfo>;

    /// Begin capturing. Frames are delivered to `sink` on an internal thread.
    fn start(&mut self, sink: FrameSink) -> Result<(), CaptureError>;

    /// Stop capturing and release all resources.
    fn stop(&mut self);
}

/// Select and open the best capture backend for the current session.
pub fn create_source() -> Result<Box<dyn FrameSource>, CaptureError> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    match session.as_str() {
        "wayland" => open_wayland(),
        "x11" => open_x11(),
        other => {
            // Unknown/empty session: try Wayland, then X11.
            open_wayland().or_else(|_| open_x11()).map_err(|_| {
                CaptureError::Unsupported(format!(
                    "no capture backend available for session '{other}' \
                     (build with --features capture-wayland,capture-x11)"
                ))
            })
        }
    }
}

#[cfg(feature = "capture-wayland")]
fn open_wayland() -> Result<Box<dyn FrameSource>, CaptureError> {
    wayland::open()
}

#[cfg(not(feature = "capture-wayland"))]
fn open_wayland() -> Result<Box<dyn FrameSource>, CaptureError> {
    Err(CaptureError::Unsupported(
        "wayland capture not built (enable feature `capture-wayland`)".into(),
    ))
}

#[cfg(feature = "capture-x11")]
fn open_x11() -> Result<Box<dyn FrameSource>, CaptureError> {
    x11::open()
}

#[cfg(not(feature = "capture-x11"))]
fn open_x11() -> Result<Box<dyn FrameSource>, CaptureError> {
    Err(CaptureError::Unsupported(
        "x11 capture not built (enable feature `capture-x11`)".into(),
    ))
}
