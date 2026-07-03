//! Runtime configuration.
//!
//! Loaded from a simple TOML file in a future iteration. Defaults are chosen to
//! be safe and local-first: clips go to a `clips/` folder next to the binary,
//! nothing leaves the machine.

use std::path::PathBuf;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buffer_seconds: 30,
            target_fps: 60,
            output_dir: PathBuf::from("clips"),
            save_hotkey: "Ctrl+Alt+S".to_string(),
        }
    }
}
