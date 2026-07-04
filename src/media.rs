//! Shared media types used across capture, encode, and buffering.
//!
//! These are dependency-free (std only) so the core compiles on any host; the
//! feature-gated Linux backends (PipeWire, X11, GStreamer) build on top of them.

use std::sync::Arc;

/// Pixel format of a captured frame. All variants are 32-bit packed RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx8888,
    Rgbx8888,
    Bgra8888,
    Rgba8888,
    Xrgb8888,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        4
    }

    /// The GStreamer `video/x-raw` format string for this pixel layout.
    pub fn gst_format(self) -> &'static str {
        match self {
            PixelFormat::Bgrx8888 => "BGRx",
            PixelFormat::Rgbx8888 => "RGBx",
            PixelFormat::Bgra8888 => "BGRA",
            PixelFormat::Rgba8888 => "RGBA",
            PixelFormat::Xrgb8888 => "xRGB",
        }
    }
}

/// What the capture backend should grab.
///
/// The default is [`CaptureTarget::Monitor`] (the whole screen / X11 root
/// window), preserving the original behavior. The window modes address the
/// origin-thread request to record a single window and re-attach to it across
/// relaunches:
///
/// - **X11** re-finds the chosen window by its `WM_CLASS`/title (persisted to
///   `~/.config/rewind/window.target`) and uses XComposite so occluded windows
///   still capture; [`CaptureTarget::ActiveWindow`] resolves `_NET_ACTIVE_WINDOW`.
/// - **Wayland** maps window modes to the portal's `SourceType::Window`; the
///   persisted `restore_token` re-attaches to the same share target without a
///   re-prompt. The portal has no active-window concept, so `ActiveWindow`
///   behaves like `Window` there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The whole monitor / X11 root window (default).
    Monitor,
    /// A specific window, re-found across relaunches.
    Window,
    /// Whichever window is active when capture starts.
    ActiveWindow,
}

impl CaptureTarget {
    /// Whether this target captures a single window (either explicitly chosen or
    /// the active one) rather than the whole monitor.
    pub fn is_window(self) -> bool {
        matches!(self, CaptureTarget::Window | CaptureTarget::ActiveWindow)
    }
}

/// Negotiated properties of an active capture stream.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub format: PixelFormat,
}

/// A single captured frame with CPU-accessible pixels.
///
/// `data` is reference-counted so a frame can be cheaply handed between the
/// capture thread and the encoder without a deep copy.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may exceed `width * bpp` due to padding).
    pub stride: u32,
    pub format: PixelFormat,
    /// Presentation timestamp in nanoseconds on a monotonic clock.
    pub pts_ns: u64,
    pub data: Arc<Vec<u8>>,
}

/// Which track an encoded packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Video,
    Audio,
}

/// An encoded, muxable chunk of media (an H.264 access unit or an AAC frame).
#[derive(Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    pub dts_ns: Option<u64>,
    pub is_keyframe: bool,
    pub track: Track,
}

/// Audio codec for the muxed track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
}

impl AudioCodec {
    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Opus => "Opus",
        }
    }
}

/// Where audio is captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// System / game audio — the monitor of the default output sink.
    SystemMonitor,
    /// The default microphone / input source.
    Microphone,
}

/// Negotiated audio-stream properties, for muxing.
#[derive(Debug, Clone, Copy)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub codec: AudioCodec,
}

/// Video codec used for the buffered/encoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// Output container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }
}

/// Encoder configuration derived from user settings.
#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub codec: Codec,
    pub bitrate_kbps: u32,
    pub container: Container,
    /// Maximum interval between keyframes, in frames. Smaller = tighter clip
    /// start alignment when flushing the ring buffer, at a small bitrate cost.
    pub keyframe_interval: u32,

    // --- audio ---
    /// Capture and mux an audio track alongside the video.
    pub capture_audio: bool,
    /// Where audio comes from (system output monitor, or the mic).
    pub audio_source: AudioSource,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,

    // --- post-save ---
    /// After saving, transcode the clip to a standard, shareable H.264/AAC MP4
    /// (faststart) as a background step.
    pub auto_convert: bool,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            bitrate_kbps: 40_000,
            container: Container::Mp4,
            keyframe_interval: 60,
            capture_audio: true,
            audio_source: AudioSource::SystemMonitor,
            audio_codec: AudioCodec::Aac,
            audio_bitrate_kbps: 160,
            auto_convert: true,
        }
    }
}
