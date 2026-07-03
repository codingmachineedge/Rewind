//! Encoding and muxing: an [`Encoder`] turns raw frames into encoded packets
//! continuously; a [`Muxer`] writes a buffered set of packets to a clip file.
//!
//! The real implementation is a GStreamer pipeline with hardware encode
//! (VA-API / NVENC, x264 fallback) behind the `encode-gstreamer` feature.

use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::media::{AudioInfo, EncodeSettings, EncodedPacket, Frame, StreamInfo};

#[cfg(feature = "encode-gstreamer")]
pub mod gstreamer;

#[derive(Debug)]
pub enum EncodeError {
    Unsupported(String),
    Backend(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Unsupported(s) => write!(f, "encode unsupported: {s}"),
            EncodeError::Backend(s) => write!(f, "encode backend error: {s}"),
        }
    }
}

impl Error for EncodeError {}

/// A continuous encoder: raw frames in, encoded packets out. When audio capture
/// is enabled, the encoder also captures + encodes audio internally and emits
/// [`crate::media::Track::Audio`]-tagged packets from `poll`/`flush`.
pub trait Encoder: Send {
    fn name(&self) -> &str;

    /// Configure the encoder for a negotiated video stream. Called once before
    /// frames. Also starts the audio branch if `settings.capture_audio`.
    fn configure(&mut self, info: StreamInfo, settings: &EncodeSettings) -> Result<(), EncodeError>;

    /// Submit a raw video frame for encoding.
    fn push_frame(&mut self, frame: &Frame) -> Result<(), EncodeError>;

    /// Drain any encoded packets that are ready (video and audio), delivering
    /// them to `sink` tagged with their [`crate::media::Track`].
    fn poll(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError>;

    /// Flush remaining packets (e.g. on shutdown).
    fn flush(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError>;

    /// Negotiated audio-track info, once configured with audio enabled.
    fn audio_info(&self) -> Option<AudioInfo>;
}

/// Muxes an ordered set of encoded packets into a container file on disk. When
/// `audio` is non-empty, an audio track is muxed alongside the video.
pub trait Muxer: Send {
    fn write_clip(
        &self,
        video: &[EncodedPacket],
        info: StreamInfo,
        audio: &[EncodedPacket],
        audio_info: Option<AudioInfo>,
        settings: &EncodeSettings,
        out: &Path,
    ) -> Result<(), EncodeError>;
}

/// Transcode a saved clip into a standard, widely-playable H.264/AAC MP4 with
/// `faststart` (moov atom relocated to the front for instant streaming). Runs as
/// a post-save step when `settings.auto_convert` is set.
pub fn convert_to_shareable(
    input: &Path,
    output: &Path,
    settings: &EncodeSettings,
) -> Result<(), EncodeError> {
    #[cfg(feature = "encode-gstreamer")]
    {
        gstreamer::convert_to_shareable(input, output, settings)
    }
    #[cfg(not(feature = "encode-gstreamer"))]
    {
        let _ = (input, output, settings);
        Err(EncodeError::Unsupported(
            "auto-convert needs the `encode-gstreamer` feature".into(),
        ))
    }
}

/// Create the encoder backend, if one is built in.
pub fn create_encoder() -> Result<Box<dyn Encoder>, EncodeError> {
    #[cfg(feature = "encode-gstreamer")]
    {
        gstreamer::encoder()
    }
    #[cfg(not(feature = "encode-gstreamer"))]
    {
        Err(EncodeError::Unsupported(
            "no encoder built (enable feature `encode-gstreamer`)".into(),
        ))
    }
}

/// Create the muxer backend, if one is built in.
pub fn create_muxer() -> Result<Box<dyn Muxer>, EncodeError> {
    #[cfg(feature = "encode-gstreamer")]
    {
        gstreamer::muxer()
    }
    #[cfg(not(feature = "encode-gstreamer"))]
    {
        Err(EncodeError::Unsupported(
            "no muxer built (enable feature `encode-gstreamer`)".into(),
        ))
    }
}
