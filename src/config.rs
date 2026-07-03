//! Runtime configuration.
//!
//! Loaded from a simple TOML file in a future iteration. Defaults are chosen to
//! be safe and local-first: clips go to a `clips/` folder next to the binary,
//! nothing leaves the machine.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::media::{Codec, Container, EncodeSettings};

#[derive(Debug, Clone)]
pub struct Config {
    /// How many seconds of gameplay to keep in the rolling buffer.
    pub buffer_seconds: u32,
    /// Target capture frame rate (frames per second).
    pub target_fps: u32,
    /// Where saved clips are written.
    pub output_dir: PathBuf,
    /// Global hotkey that flushes the buffer to a clip.
    pub save_hotkey: String,
    /// Encoder target bitrate, in kilobits per second.
    pub bitrate_kbps: u32,
    /// Output container format.
    pub container: Container,
    /// Video codec.
    pub codec: Codec,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buffer_seconds: 30,
            target_fps: 60,
            output_dir: PathBuf::from("clips"),
            save_hotkey: "Ctrl+Alt+S".to_string(),
            bitrate_kbps: 40_000,
            container: Container::Mp4,
            codec: Codec::H264,
        }
    }
}

impl Config {
    /// Derive the encoder settings for this configuration.
    pub fn encode_settings(&self) -> EncodeSettings {
        EncodeSettings {
            codec: self.codec,
            bitrate_kbps: self.bitrate_kbps,
            container: self.container,
            // A keyframe every ~2 seconds bounds how much the clip start is
            // trimmed when snapshotting the ring buffer.
            keyframe_interval: (self.target_fps * 2).max(1),
        }
    }

    /// Build a fresh, timestamped output path inside the configured folder,
    /// e.g. `clips/rewind_1751500000.mp4`.
    pub fn new_clip_path(&self) -> PathBuf {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.output_dir
            .join(format!("rewind_{secs}.{}", self.container.extension()))
    }
}
