//! ClipForge — lightweight, privacy-respecting game clip recorder.
//!
//! Target platform: Linux (Wayland via PipeWire/portal, and X11).
//!
//! This is an early scaffold. The real capture pipeline (PipeWire/portal screen
//! capture -> encoder -> ring buffer) is stubbed out; see `docs/ARCHITECTURE.md`.

mod buffer;
mod config;

use buffer::ClipBuffer;
use config::Config;

fn main() {
    println!("ClipForge v{} — privacy-first clip recorder", env!("CARGO_PKG_VERSION"));
    println!("No account. No telemetry. Your footage stays on your machine.\n");

    let config = Config::default();
    println!(
        "Config: buffer={}s, output={:?}, hotkey={}",
        config.buffer_seconds, config.output_dir, config.save_hotkey
    );

    // A ring buffer sized to hold `buffer_seconds` of frames at the target FPS.
    let mut clip_buffer = ClipBuffer::new(config.buffer_seconds, config.target_fps);
    println!(
        "Initialized ring buffer: capacity = {} frames.\n",
        clip_buffer.capacity()
    );

    // TODO: start the capture thread that pushes frames into `clip_buffer`,
    // and register the global hotkey that calls `clip_buffer.flush_to_clip()`.
    println!("[stub] Capture pipeline not yet implemented — see docs/ARCHITECTURE.md.");
    println!("[stub] Would open a PipeWire/portal ScreenCast (Wayland) or X11 capture and begin buffering.");

    // Demonstrate the buffer API so the skeleton is exercised and buildable.
    clip_buffer.push_frame_placeholder();
    match clip_buffer.flush_to_clip(&config.output_dir) {
        Ok(path) => println!("[stub] Clip would be saved to: {}", path),
        Err(e) => eprintln!("[stub] Save failed: {e}"),
    }
}
