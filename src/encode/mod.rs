//! Encoding and muxing: an [`Encoder`] turns raw frames into encoded packets
//! continuously; a [`Muxer`] writes a buffered set of packets to a clip file.
//!
//! The real implementation is a GStreamer pipeline with hardware encode
//! (VA-API / NVENC, x264 fallback) behind the `encode-gstreamer` feature.

use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::media::{EncodeSettings, EncodedPacket, Frame, StreamInfo};

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

/// A continuous encoder: raw frames in, encoded packets out.
pub trait Encoder: Send {
    fn name(&self) -> &str;

    /// Configure the encoder for a negotiated stream. Called once before frames.
    fn configure(&mut self, info: StreamInfo, settings: &EncodeSettings) -> Result<(), EncodeError>;

    /// Submit a raw frame for encoding.
    fn push_frame(&mut self, frame: &Frame) -> Result<(), EncodeError>;

    /// Drain any encoded packets that are ready, delivering them to `sink`.
    fn poll(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError>;

    /// Flush remaining packets (e.g. on shutdown).
    fn flush(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError>;
}

/// Muxes an ordered set of encoded packets into a container file on disk.
pub trait Muxer: Send {
    fn write_clip(
        &self,
        packets: &[EncodedPacket],
        info: StreamInfo,
        settings: &EncodeSettings,
        out: &Path,
    ) -> Result<(), EncodeError>;
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
