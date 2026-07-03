//! Rewind — lightweight, privacy-respecting game clip recorder.
//!
//! Target platform: Linux (Wayland via PipeWire/portal, and X11).
//!
//! Architecture (see `docs/ARCHITECTURE.md`):
//!   capture ([`capture::FrameSource`]) -> encode ([`encode::Encoder`])
//!   -> rolling [`buffer::ClipBuffer`] -> save/mux ([`encode::Muxer`]),
//!   orchestrated by [`pipeline::Pipeline`] and driven by the GUI or CLI.
//!
//! The core (buffer, config, traits, orchestrator) is dependency-free and builds
//! on any host. The GTK GUI and the Linux capture/encode/hotkey backends are
//! behind cargo features so the default build stays green everywhere.
//!
//! Build the full Linux app with:  `cargo run --features linux`

// Feature-gated backends leave several core APIs unused in the default build.
#![allow(dead_code)]

mod buffer;
mod capture;
mod config;
mod encode;
mod hotkey;
mod media;
mod pipeline;

#[cfg(feature = "gui")]
mod gui;

/// GUI entry point (GTK4 + libadwaita).
#[cfg(feature = "gui")]
fn main() {
    gui::run();
}

/// Headless CLI entry point — used when built without the `gui` feature.
#[cfg(not(feature = "gui"))]
fn main() {
    use std::sync::Arc;
    use std::time::Duration;

    use config::Config;
    use pipeline::{Pipeline, PipelineEvent};

    println!("Rewind v{} — privacy-first clip recorder", env!("CARGO_PKG_VERSION"));
    println!("No account. No telemetry. Your footage stays on your machine.\n");

    let config = Config::default();
    println!(
        "Config: buffer={}s, fps={}, output={:?}, hotkey={}",
        config.buffer_seconds, config.target_fps, config.output_dir, config.save_hotkey
    );

    let events: pipeline::EventSink = Arc::new(|event: PipelineEvent| match event {
        PipelineEvent::Status(s) => println!("[status] {s}"),
        PipelineEvent::ClipSaved(p) => println!("[saved]  clip written to {}", p.display()),
        PipelineEvent::Error(e) => eprintln!("[error]  {e}"),
    });

    let mut pipeline = Pipeline::new(config, events);
    match pipeline.start() {
        Ok(()) if pipeline.is_running() => {
            println!("\nCapturing. Recording the rolling buffer for a few seconds…");
            std::thread::sleep(Duration::from_secs(3));
            println!("Flushing the last N seconds to a clip…");
            pipeline.save_last_n();
            std::thread::sleep(Duration::from_secs(1));
            pipeline.stop();
        }
        Ok(()) => println!("Pipeline did not start capturing."),
        Err(e) => {
            eprintln!("\n[error] could not start capture: {e}");
            eprintln!("This build has no Linux capture backend compiled in.");
            eprintln!("On Linux, build the real app with: cargo run --features linux");
        }
    }
}
