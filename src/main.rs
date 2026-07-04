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

mod autostart;
mod buffer;
mod capture;
mod clipboard;
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
    // Handle --install-autostart / --uninstall-autostart before GTK sees argv.
    if autostart::handle_cli_args() {
        return;
    }
    gui::run();
}

/// Headless CLI entry point — used when built without the `gui` feature.
#[cfg(not(feature = "gui"))]
fn main() {
    use std::sync::Arc;
    use std::time::Duration;

    if autostart::handle_cli_args() {
        return;
    }

    use config::Config;
    use pipeline::{Pipeline, PipelineEvent};

    println!("Rewind v{} — privacy-first clip recorder", env!("CARGO_PKG_VERSION"));
    println!("No account. No telemetry. Your footage stays on your machine.\n");

    let mut config = Config::default();
    // Opt-in clipboard copy for headless runs: REWIND_CLIPBOARD=1
    config.copy_to_clipboard = std::env::var("REWIND_CLIPBOARD").is_ok();
    println!(
        "Config: buffer={}s, fps={}, output={:?}, hotkey={}, clipboard={}",
        config.buffer_seconds,
        config.target_fps,
        config.output_dir,
        config.save_hotkey,
        config.copy_to_clipboard
    );

    let events: pipeline::EventSink = Arc::new(|event: PipelineEvent| match event {
        PipelineEvent::Status(s) => println!("[status] {s}"),
        PipelineEvent::ClipSaved(p) => println!("[saved]  clip written to {}", p.display()),
        PipelineEvent::ClipConverted(p) => println!("[share]  converted to {}", p.display()),
        PipelineEvent::Error(e) => eprintln!("[error]  {e}"),
    });

    let mut pipeline = Pipeline::new(config, events);
    match pipeline.start() {
        Ok(()) if pipeline.is_running() => {
            let secs: u64 = std::env::var("REWIND_CAPTURE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            println!("\nCapturing. Buffering the rolling window for {secs}s…");
            std::thread::sleep(Duration::from_secs(secs));
            println!("Flushing the last N seconds to a clip…");
            pipeline.save_last_n();
            // Allow the background mux + auto-convert to finish.
            std::thread::sleep(Duration::from_secs(15));
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
