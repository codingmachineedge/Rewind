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
use crate::media::{AudioInfo, EncodedPacket, Frame, StreamInfo, Track};
use crate::{capture, encode, hotkey};

/// Events the pipeline emits to its front-end.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Status(String),
    ClipSaved(PathBuf),
    /// The auto-converted, shareable copy finished writing.
    ClipConverted(PathBuf),
    Error(String),
}

/// Thread-safe event callback.
pub type EventSink = Arc<dyn Fn(PipelineEvent) + Send + Sync>;

/// Shared, cloneable core accessed from every worker thread and the front-end.
pub struct PipelineCore {
    pub buffer: Mutex<ClipBuffer>,
    /// Parallel ring buffer of encoded audio packets (when audio is enabled).
    pub audio_buffer: Mutex<ClipBuffer>,
    pub config: Mutex<Config>,
    stream_info: Mutex<Option<StreamInfo>>,
    audio_info: Mutex<Option<AudioInfo>>,
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

        let video = self.buffer.lock().unwrap().snapshot();
        if video.is_empty() {
            self.emit(PipelineEvent::Error("buffer is empty".into()));
            self.saving.store(false, Ordering::Release);
            return;
        }

        // Audio packets covering the same window (AAC frames are all keyframes,
        // so the snapshot is the whole buffered span).
        let audio: Vec<EncodedPacket> = self.audio_buffer.lock().unwrap().snapshot();
        let audio_info = *self.audio_info.lock().unwrap();

        let (settings, out) = {
            let cfg = self.config.lock().unwrap();
            (cfg.encode_settings(), cfg.new_clip_path())
        };

        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let this = self.clone();
        thread::spawn(move || {
            let audio_slice: &[EncodedPacket] = if audio_info.is_some() { &audio } else { &[] };
            let result = match encode::create_muxer() {
                Ok(muxer) => {
                    muxer.write_clip(&video, info, audio_slice, audio_info, &settings, &out)
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    let had_audio = !audio_slice.is_empty();
                    this.emit(PipelineEvent::ClipSaved(out.clone()));
                    this.emit(PipelineEvent::Status(format!(
                        "clip saved ({} video, {} audio packets){}",
                        video.len(),
                        audio_slice.len(),
                        if had_audio { "" } else { " — no audio track" }
                    )));

                    // Post-save: auto-convert to a shareable H.264/AAC MP4.
                    if settings.auto_convert {
                        let share = Config::shareable_path(&out);
                        this.emit(PipelineEvent::Status(format!(
                            "converting → {}",
                            share.display()
                        )));
                        match encode::convert_to_shareable(&out, &share, &settings) {
                            Ok(()) => this.emit(PipelineEvent::ClipConverted(share)),
                            Err(e) => this.emit(PipelineEvent::Error(format!(
                                "auto-convert failed: {e}"
                            ))),
                        }
                    }
                }
                Err(e) => this.emit(PipelineEvent::Error(format!("save failed: {e}"))),
            }
            this.saving.store(false, Ordering::Release);
        });
    }
}

/// A packet sink that routes encoded packets to the correct ring buffer by track.
fn route_sink(core: Arc<PipelineCore>) -> impl FnMut(EncodedPacket) {
    move |pkt: EncodedPacket| match pkt.track {
        Track::Video => core.buffer.lock().unwrap().push(pkt),
        Track::Audio => core.audio_buffer.lock().unwrap().push(pkt),
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
        // Audio produces far fewer packets/sec than video; size generously.
        let audio_buffer = ClipBuffer::new(config.buffer_seconds, 100);
        let core = Arc::new(PipelineCore {
            buffer: Mutex::new(buffer),
            audio_buffer: Mutex::new(audio_buffer),
            config: Mutex::new(config),
            stream_info: Mutex::new(None),
            audio_info: Mutex::new(None),
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

        // Re-create the buffers to match current settings.
        let (fps, settings, accelerator) = {
            let cfg = self.core.config.lock().unwrap();
            *self.core.buffer.lock().unwrap() =
                ClipBuffer::new(cfg.buffer_seconds, cfg.target_fps);
            *self.core.audio_buffer.lock().unwrap() = ClipBuffer::new(cfg.buffer_seconds, 100);
            (cfg.target_fps, cfg.encode_settings(), cfg.save_hotkey.clone())
        };
        *self.core.stream_info.lock().unwrap() = None;
        *self.core.audio_info.lock().unwrap() = None;

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
                                    *core.audio_info.lock().unwrap() = enc.audio_info();
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
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                // Drain encoded packets (video + audio) each iteration, so audio
                // keeps flowing even when video frames arrive slowly.
                if configured {
                    if let Some(enc) = encoder.as_mut() {
                        let core2 = core.clone();
                        let mut sink = route_sink(core2);
                        let _ = enc.poll(&mut sink);
                    }
                }
            }

            if let Some(enc) = encoder.as_mut() {
                let core2 = core.clone();
                let mut sink = route_sink(core2);
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
