//! Runtime configuration.
//!
//! Loaded from a simple TOML file in a future iteration. Defaults are chosen to
//! be safe and local-first: clips go to a `clips/` folder next to the binary,
//! nothing leaves the machine.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::media::{AudioCodec, AudioSource, Codec, Container, EncodeSettings};

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

    /// Capture and mux an audio track alongside video.
    pub capture_audio: bool,
    /// Where audio is captured from (system output, or mic).
    pub audio_source: AudioSource,
    /// Audio bitrate, in kilobits per second.
    pub audio_bitrate_kbps: u32,

    /// After saving, auto-convert the clip to a shareable H.264/AAC MP4.
    pub auto_convert: bool,
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
            capture_audio: true,
            audio_source: AudioSource::SystemMonitor,
            audio_bitrate_kbps: 160,
            auto_convert: true,
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
            capture_audio: self.capture_audio,
            audio_source: self.audio_source,
            // AAC in MP4 is the universally playable default; Opus pairs with MKV.
            audio_codec: match self.container {
                Container::Mkv => AudioCodec::Opus,
                Container::Mp4 => AudioCodec::Aac,
            },
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            auto_convert: self.auto_convert,
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

    /// The shareable output path for the auto-converted copy of `clip`, e.g.
    /// `clips/rewind_1751500000.share.mp4`.
    pub fn shareable_path(clip: &std::path::Path) -> PathBuf {
        let stem = clip.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
        let dir = clip.parent().unwrap_or_else(|| std::path::Path::new("."));
        dir.join(format!("{stem}.share.mp4"))
    }
}
