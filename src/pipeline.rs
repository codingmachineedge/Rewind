//! The runtime pipeline: capture → encode → rolling buffer → save.
//!
//! [`Pipeline`] owns the lifecycle (start/stop, capture source, hotkeys, encode
//! worker). The shared [`PipelineCore`] holds the buffer, config, and negotiated
//! stream info, and exposes [`PipelineCore::save_last_n`] — callable from both
//! the GUI button and the global hotkey. Events flow back out through a
//! thread-safe callback so any front-end (GUI or CLI) can react.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::buffer::ClipBuffer;
use crate::config::Config;
use crate::media::{Frame, StreamInfo};
use crate::{capture, encode, hotkey};

/// Events the pipeline emits to its front-end.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Status(String),
    ClipSaved(PathBuf),
    Error(String),
}

/// Thread-safe event callback.
pub type EventSink = Arc<dyn Fn(PipelineEvent) + Send + Sync>;

/// Shared, cloneable core accessed from every worker thread and the front-end.
pub struct PipelineCore {
    pub buffer: Mutex<ClipBuffer>,
    pub config: Mutex<Config>,
    stream_info: Mutex<Option<StreamInfo>>,
    saving: AtomicBool,
    events: EventSink,
}

impl PipelineCore {
    fn emit(&self, event: PipelineEvent) {
        (self.events)(event);
    }

    fn set_stream_info(&self, info: StreamInfo) {
        *self.stream_info.lock().unwrap() = Some(info);
    }

    /// Flush the rolling buffer to a timestamped clip file. Non-blocking: the
    /// mux runs on a background thread and reports completion via an event.
    pub fn save_last_n(self: &Arc<Self>) {
        if self.saving.swap(true, Ordering::AcqRel) {
            self.emit(PipelineEvent::Status("save already in progress".into()));
            return;
        }

        let info = match *self.stream_info.lock().unwrap() {
            Some(info) => info,
            None => {
                self.emit(PipelineEvent::Error(
                    "nothing to save yet — capture is not running".into(),
                ));
                self.saving.store(false, Ordering::Release);
                return;
            }
        };

        let packets = self.buffer.lock().unwrap().snapshot();
        if packets.is_empty() {
            self.emit(PipelineEvent::Error("buffer is empty".into()));
            self.saving.store(false, Ordering::Release);
            return;
        }

        let (settings, out) = {
            let cfg = self.config.lock().unwrap();
            (cfg.encode_settings(), cfg.new_clip_path())
        };

        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let this = self.clone();
        thread::spawn(move || {
            let result = match encode::create_muxer() {
                Ok(muxer) => muxer.write_clip(&packets, info, &settings, &out),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => this.emit(PipelineEvent::ClipSaved(out)),
                Err(e) => this.emit(PipelineEvent::Error(format!("save failed: {e}"))),
            }
            this.saving.store(false, Ordering::Release);
        });
    }
}

/// Owns the running pipeline and its threads.
pub struct Pipeline {
    core: Arc<PipelineCore>,
    running: Arc<AtomicBool>,
    source: Option<Box<dyn capture::FrameSource>>,
    hotkeys: Option<Box<dyn hotkey::HotkeyManager>>,
    worker: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn new(config: Config, events: EventSink) -> Self {
        let buffer = ClipBuffer::new(config.buffer_seconds, config.target_fps);
        let core = Arc::new(PipelineCore {
            buffer: Mutex::new(buffer),
            config: Mutex::new(config),
            stream_info: Mutex::new(None),
            saving: AtomicBool::new(false),
            events,
        });
        Self {
            core,
            running: Arc::new(AtomicBool::new(false)),
            source: None,
            hotkeys: None,
            worker: None,
        }
    }

    /// Shared handle for the front-end (e.g. to trigger a save from a button).
    pub fn core(&self) -> Arc<PipelineCore> {
        self.core.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Start capture + continuous encoding into the rolling buffer, and register
    /// the global save hotkey. Resizes the buffer to the current settings first.
    pub fn start(&mut self) -> Result<(), String> {
        if self.is_running() {
            return Ok(());
        }

        // Re-create the buffer to match current settings.
        let (fps, settings, accelerator) = {
            let cfg = self.core.config.lock().unwrap();
            *self.core.buffer.lock().unwrap() =
                ClipBuffer::new(cfg.buffer_seconds, cfg.target_fps);
            (cfg.target_fps, cfg.encode_settings(), cfg.save_hotkey.clone())
        };
        *self.core.stream_info.lock().unwrap() = None;

        self.running.store(true, Ordering::Release);
        let (tx, rx) = mpsc::channel::<Frame>();

        // Encode worker: raw frames in, encoded packets into the ring buffer.
        let core = self.core.clone();
        let running = self.running.clone();
        self.worker = Some(thread::spawn(move || {
            let mut encoder = match encode::create_encoder() {
                Ok(enc) => Some(enc),
                Err(e) => {
                    core.emit(PipelineEvent::Status(format!(
                        "capture running without an encoder: {e}"
                    )));
                    None
                }
            };
            let mut configured = false;

            while running.load(Ordering::Acquire) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(frame) => {
                        let Some(enc) = encoder.as_mut() else { continue };
                        if !configured {
                            let info = StreamInfo {
                                width: frame.width,
                                height: frame.height,
                                framerate: fps,
                                format: frame.format,
                            };
                            match enc.configure(info, &settings) {
                                Ok(()) => {
                                    core.set_stream_info(info);
                                    configured = true;
                                }
                                Err(e) => {
                                    core.emit(PipelineEvent::Error(format!(
                                        "encoder configure failed: {e}"
                                    )));
                                    continue;
                                }
                            }
                        }
                        if let Err(e) = enc.push_frame(&frame) {
                            core.emit(PipelineEvent::Error(format!("encode error: {e}")));
                            continue;
                        }
                        let core2 = core.clone();
                        let mut sink = move |pkt| {
                            core2.buffer.lock().unwrap().push(pkt);
                        };
                        let _ = enc.poll(&mut sink);
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            if let Some(enc) = encoder.as_mut() {
                let core2 = core.clone();
                let mut sink = move |pkt| {
                    core2.buffer.lock().unwrap().push(pkt);
                };
                let _ = enc.flush(&mut sink);
            }
        }));

        // Capture source: push frames into the encode channel.
        let mut source = capture::create_source().map_err(|e| {
            self.running.store(false, Ordering::Release);
            e.to_string()
        })?;
        let sink_tx = tx;
        if let Err(e) = source.start(Box::new(move |frame| {
            let _ = sink_tx.send(frame);
        })) {
            self.running.store(false, Ordering::Release);
            return Err(e.to_string());
        }
        self.core.emit(PipelineEvent::Status(format!(
            "capturing via {} — buffering last {}s",
            source.name(),
            self.core.config.lock().unwrap().buffer_seconds
        )));
        self.source = Some(source);

        // Global save hotkey (best-effort; failure is non-fatal).
        match hotkey::create_manager() {
            Ok(mut hk) => {
                let core = self.core.clone();
                let register = hk.register_save(
                    &accelerator,
                    Box::new(move || core.save_last_n()),
                );
                match register {
                    Ok(()) => {
                        self.core.emit(PipelineEvent::Status(format!(
                            "save hotkey registered ({accelerator}) via {}",
                            hk.name()
                        )));
                        self.hotkeys = Some(hk);
                    }
                    Err(e) => self.core.emit(PipelineEvent::Status(format!(
                        "hotkey unavailable: {e} — use the Save button"
                    ))),
                }
            }
            Err(e) => self.core.emit(PipelineEvent::Status(format!(
                "hotkey backend unavailable: {e} — use the Save button"
            ))),
        }

        Ok(())
    }

    /// Stop capture and tear down the worker + hotkeys.
    pub fn stop(&mut self) {
        if !self.is_running() {
            return;
        }
        self.running.store(false, Ordering::Release);
        if let Some(mut source) = self.source.take() {
            source.stop();
        }
        if let Some(mut hk) = self.hotkeys.take() {
            hk.stop();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.core.emit(PipelineEvent::Status("capture stopped".into()));
    }

    /// Convenience: trigger a save via the shared core.
    pub fn save_last_n(&self) {
        self.core.save_last_n();
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}
