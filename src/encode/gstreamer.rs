//! GStreamer encode/mux backend (feature `encode-gstreamer`, Linux only).
//!
//! Three pieces:
//!
//! * [`GstEncoder`] runs a live pipeline that encodes video —
//!   `appsrc ! videoconvert ! <hwenc|x264enc> ! h264parse ! appsink` — and, when
//!   audio is enabled, a parallel branch that captures + encodes audio —
//!   `pulsesrc ! audioconvert ! audioresample ! <aacenc|opusenc> ! parse ! appsink`.
//!   Both branches share the pipeline clock (video uses `do-timestamp`, audio is
//!   a live source) so the two packet streams stay A/V-aligned.
//! * [`GstMuxer`] muxes buffered video (+ audio) packets into an MP4/MKV file.
//! * [`convert_to_shareable`] transcodes a saved clip to a standard H.264/AAC MP4
//!   with `faststart` for sharing.
//!
//! Encoders are selected at runtime by probing the GStreamer registry, preferring
//! VA-API, then NVENC, then software (`x264enc` / `x265enc`).
//!
//! Targets `gstreamer-rs` 0.23. Compile- and run-verified on Linux with GStreamer
//! plus the good/bad/libav plugin sets installed.

use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::encode::{EncodeError, Encoder, Muxer};
use crate::media::{
    AudioCodec, AudioInfo, AudioSource, Codec, Container, EncodeSettings, EncodedPacket, Frame,
    StreamInfo, Track,
};

/// Initialize GStreamer. Idempotent: safe to call from every entry point.
fn init() -> Result<(), EncodeError> {
    gst::init().map_err(|e| EncodeError::Backend(format!("gst::init failed: {e}")))
}

fn backend<E: std::fmt::Display>(ctx: &str, e: E) -> EncodeError {
    EncodeError::Backend(format!("{ctx}: {e}"))
}

fn have_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// First registered element from a candidate list.
fn first_available(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|n| have_element(n))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn encoder() -> Result<Box<dyn Encoder>, EncodeError> {
    init()?;
    Ok(Box::new(GstEncoder::new()))
}

pub fn muxer() -> Result<Box<dyn Muxer>, EncodeError> {
    init()?;
    Ok(Box::new(GstMuxer))
}

// ---------------------------------------------------------------------------
// Element selection
// ---------------------------------------------------------------------------

struct EncoderChoice {
    enc: &'static str,
    parse: &'static str,
    media_type: &'static str,
    is_software: bool,
}

fn choose_encoder(codec: Codec) -> Result<EncoderChoice, EncodeError> {
    let (candidates, parse, media_type, software): (&[&str], &str, &str, &str) = match codec {
        Codec::H264 => (
            &["vaapih264enc", "nvh264enc", "x264enc"],
            "h264parse",
            "video/x-h264",
            "x264enc",
        ),
        Codec::Hevc => (
            &["vaapih265enc", "nvh265enc", "x265enc"],
            "h265parse",
            "video/x-h265",
            "x265enc",
        ),
    };
    match first_available(candidates) {
        Some(enc) => Ok(EncoderChoice {
            enc,
            parse,
            media_type,
            is_software: enc == software,
        }),
        None => Err(EncodeError::Unsupported(format!(
            "no GStreamer encoder available for {codec:?} (tried {candidates:?})"
        ))),
    }
}

struct AudioChoice {
    enc: &'static str,
    /// Parser element between encoder and sink/mux, if any.
    parse: Option<&'static str>,
    /// Caps for the appsink / mux appsrc.
    caps: gst::Caps,
}

fn choose_audio_encoder(codec: AudioCodec) -> Result<AudioChoice, EncodeError> {
    match codec {
        AudioCodec::Aac => {
            let enc = first_available(&["avenc_aac", "fdkaacenc", "voaacenc"]).ok_or_else(|| {
                EncodeError::Unsupported("no AAC encoder (avenc_aac/fdkaacenc/voaacenc)".into())
            })?;
            // ADTS is self-framing, so stored packets need no external codec_data.
            let caps = gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "adts")
                .build();
            Ok(AudioChoice {
                enc,
                parse: Some("aacparse"),
                caps,
            })
        }
        AudioCodec::Opus => {
            let enc = first_available(&["opusenc"])
                .ok_or_else(|| EncodeError::Unsupported("no Opus encoder (opusenc)".into()))?;
            let caps = gst::Caps::builder("audio/x-opus").build();
            Ok(AudioChoice {
                enc,
                parse: Some("opusparse"),
                caps,
            })
        }
    }
}

/// Resolve the PulseAudio/PipeWire device string for a capture source.
/// For system audio we target the default sink's monitor via `pactl`.
fn resolve_audio_device(source: AudioSource) -> Option<String> {
    match source {
        AudioSource::Microphone => None, // pulsesrc default == default source (mic)
        AudioSource::SystemMonitor => {
            let out = std::process::Command::new("pactl")
                .arg("get-default-sink")
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sink.is_empty() {
                None
            } else {
                Some(format!("{sink}.monitor"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Continuous encoder
// ---------------------------------------------------------------------------

pub struct GstEncoder {
    inner: Mutex<Option<EncoderInner>>,
    audio: Mutex<Option<AudioInfo>>,
    name: String,
}

struct EncoderInner {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    appsink: gst_app::AppSink,
    appsink_audio: Option<gst_app::AppSink>,
    eos_sent: bool,
    /// First video frame's monotonic pts, so we can rebase video to a 0-based
    /// timeline (the audio branch is already ~0-based on pipeline running-time).
    base_pts: Option<u64>,
}

impl GstEncoder {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            audio: Mutex::new(None),
            name: "gstreamer".to_string(),
        }
    }
}

impl Encoder for GstEncoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn configure(&mut self, info: StreamInfo, settings: &EncodeSettings) -> Result<(), EncodeError> {
        let choice = choose_encoder(settings.codec)?;
        let pipeline = gst::Pipeline::new();

        // --- video branch -----------------------------------------------------
        let src_caps = gst::Caps::builder("video/x-raw")
            .field("format", info.format.gst_format())
            .field("width", info.width as i32)
            .field("height", info.height as i32)
            .field("framerate", gst::Fraction::new(info.framerate as i32, 1))
            .build();

        let appsrc = gst_app::AppSrc::builder()
            .caps(&src_caps)
            .is_live(true)
            .format(gst::Format::Time)
            .build();
        // We set an explicit, rebased PTS on each frame in `push_frame` (see
        // `base_pts`), so do NOT let appsrc overwrite it with arrival time.
        appsrc.set_do_timestamp(false);

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| backend("make videoconvert", e))?;
        let encoder = gst::ElementFactory::make(choice.enc)
            .build()
            .map_err(|e| backend(&format!("make {}", choice.enc), e))?;

        let _ = try_set_u32(&encoder, "bitrate", settings.bitrate_kbps);
        if choice.is_software {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                encoder.set_property_from_str("tune", "zerolatency");
            }));
            let _ = try_set_u32(&encoder, "key-int-max", settings.keyframe_interval);
        } else {
            let _ = try_set_u32(&encoder, "key-int-max", settings.keyframe_interval);
            let _ = try_set_u32(&encoder, "keyframe-period", settings.keyframe_interval);
            let _ = try_set_u32(&encoder, "gop-size", settings.keyframe_interval);
        }

        let parser = gst::ElementFactory::make(choice.parse)
            .build()
            .map_err(|e| backend(&format!("make {}", choice.parse), e))?;

        let sink_caps = gst::Caps::builder(choice.media_type)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let appsink = gst_app::AppSink::builder().caps(&sink_caps).build();
        appsink.set_max_buffers(0);
        appsink.set_drop(false);

        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &videoconvert,
                &encoder,
                &parser,
                appsink.upcast_ref::<gst::Element>(),
            ])
            .map_err(|e| backend("pipeline add_many (video)", e))?;
        gst::Element::link_many([
            appsrc.upcast_ref::<gst::Element>(),
            &videoconvert,
            &encoder,
            &parser,
            appsink.upcast_ref::<gst::Element>(),
        ])
        .map_err(|e| backend("link video chain", e))?;

        // --- audio branch (optional) -----------------------------------------
        let mut appsink_audio = None;
        let mut audio_info = None;
        if settings.capture_audio {
            match self.build_audio_branch(&pipeline, settings) {
                Ok((sink, ai)) => {
                    appsink_audio = Some(sink);
                    audio_info = Some(ai);
                }
                Err(e) => {
                    // Audio is best-effort: log via name suffix, keep video-only.
                    eprintln!("[gstreamer] audio branch disabled: {e}");
                }
            }
        }
        *self.audio.lock().unwrap() = audio_info;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| backend("set encode pipeline Playing", e))?;

        self.name = match &audio_info {
            Some(ai) => format!("gstreamer:{}+{}", choice.enc, ai.codec.label()),
            None => format!("gstreamer:{}", choice.enc),
        };
        *self.inner.lock().unwrap() = Some(EncoderInner {
            pipeline,
            appsrc,
            appsink,
            appsink_audio,
            eos_sent: false,
            base_pts: None,
        });
        Ok(())
    }

    fn push_frame(&mut self, frame: &Frame) -> Result<(), EncodeError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("push_frame before configure".into()))?;
        // Rebase to a 0-based timeline off the first frame, and set an explicit
        // PTS so the muxer always has valid timestamps.
        let base = *inner.base_pts.get_or_insert(frame.pts_ns);
        let rel = frame.pts_ns.saturating_sub(base);
        let mut buffer = gst::Buffer::from_slice(frame.data.as_slice().to_vec());
        {
            let buf = buffer
                .get_mut()
                .ok_or_else(|| EncodeError::Backend("fresh frame buffer shared".into()))?;
            buf.set_pts(gst::ClockTime::from_nseconds(rel));
        }
        inner
            .appsrc
            .push_buffer(buffer)
            .map_err(|e| backend("appsrc push_buffer", e))?;
        Ok(())
    }

    fn poll(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("poll before configure".into()))?;
        drain(&inner.appsink, gst::ClockTime::ZERO, Track::Video, sink)?;
        if let Some(a) = &inner.appsink_audio {
            drain(a, gst::ClockTime::ZERO, Track::Audio, sink)?;
        }
        Ok(())
    }

    fn flush(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("flush before configure".into()))?;

        if !inner.eos_sent {
            inner
                .appsrc
                .end_of_stream()
                .map_err(|e| backend("appsrc end_of_stream", e))?;
            inner.eos_sent = true;
        }

        // Video: EOS propagates, so a timed drain ends when the sink returns None.
        drain(&inner.appsink, gst::ClockTime::from_mseconds(100), Track::Video, sink)?;
        // Audio: the source is LIVE and never sends EOS, so a timed drain would
        // loop forever. Grab only the currently-queued backlog (non-blocking,
        // bounded) before tearing down.
        if let Some(a) = &inner.appsink_audio {
            drain_bounded(a, Track::Audio, sink, 4096)?;
        }

        inner
            .pipeline
            .set_state(gst::State::Null)
            .map_err(|e| backend("set encode pipeline Null", e))?;
        Ok(())
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        *self.audio.lock().unwrap()
    }
}

impl GstEncoder {
    /// Build + add the live audio capture/encode branch to `pipeline`, returning
    /// the audio appsink and negotiated info.
    fn build_audio_branch(
        &self,
        pipeline: &gst::Pipeline,
        settings: &EncodeSettings,
    ) -> Result<(gst_app::AppSink, AudioInfo), EncodeError> {
        let src_name = first_available(&["pulsesrc", "pipewiresrc"])
            .ok_or_else(|| EncodeError::Unsupported("no audio source (pulsesrc/pipewiresrc)".into()))?;
        let achoice = choose_audio_encoder(settings.audio_codec)?;

        let asrc = gst::ElementFactory::make(src_name)
            .build()
            .map_err(|e| backend(&format!("make {src_name}"), e))?;
        if src_name == "pulsesrc" {
            if let Some(dev) = resolve_audio_device(settings.audio_source) {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    asrc.set_property("device", dev);
                }));
            }
        }

        let aconv = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| backend("make audioconvert", e))?;
        let ares = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|e| backend("make audioresample", e))?;
        let aenc = gst::ElementFactory::make(achoice.enc)
            .build()
            .map_err(|e| backend(&format!("make {}", achoice.enc), e))?;
        // Audio bitrate props are in bits/sec. avenc_aac/opusenc declare it as
        // `gint`, so try i32 first, then u32 for encoders that use guint.
        let abits = settings.audio_bitrate_kbps.saturating_mul(1000);
        if !try_set_i32(&aenc, "bitrate", abits as i32) {
            let _ = try_set_u32(&aenc, "bitrate", abits);
        }

        let aparse = match achoice.parse {
            Some(p) => Some(
                gst::ElementFactory::make(p)
                    .build()
                    .map_err(|e| backend(&format!("make {p}"), e))?,
            ),
            None => None,
        };

        let asink = gst_app::AppSink::builder().caps(&achoice.caps).build();
        asink.set_max_buffers(0);
        asink.set_drop(false);

        // Assemble the branch.
        let mut chain: Vec<&gst::Element> = vec![&asrc, &aconv, &ares, &aenc];
        if let Some(p) = &aparse {
            chain.push(p);
        }
        let asink_el = asink.upcast_ref::<gst::Element>();
        chain.push(asink_el);

        pipeline
            .add_many(chain.iter().copied())
            .map_err(|e| backend("pipeline add_many (audio)", e))?;
        gst::Element::link_many(chain.iter().copied())
            .map_err(|e| backend("link audio chain", e))?;

        let info = AudioInfo {
            sample_rate: 48_000,
            channels: 2,
            codec: settings.audio_codec,
        };
        Ok((asink, info))
    }
}

/// Drain ready samples from an appsink, tagging each with `track`.
fn drain(
    appsink: &gst_app::AppSink,
    timeout: gst::ClockTime,
    track: Track,
    sink: &mut dyn FnMut(EncodedPacket),
) -> Result<(), EncodeError> {
    while let Some(sample) = appsink.try_pull_sample(Some(timeout)) {
        if let Some(pkt) = sample_to_packet(&sample, track)? {
            sink(pkt);
        }
    }
    Ok(())
}

/// Non-blocking drain of at most `max` currently-queued samples. Used for the
/// live audio sink, which never sends EOS (a timed drain would never return).
fn drain_bounded(
    appsink: &gst_app::AppSink,
    track: Track,
    sink: &mut dyn FnMut(EncodedPacket),
    max: usize,
) -> Result<(), EncodeError> {
    for _ in 0..max {
        match appsink.try_pull_sample(Some(gst::ClockTime::ZERO)) {
            Some(sample) => {
                if let Some(pkt) = sample_to_packet(&sample, track)? {
                    sink(pkt);
                }
            }
            None => break,
        }
    }
    Ok(())
}

fn sample_to_packet(sample: &gst::Sample, track: Track) -> Result<Option<EncodedPacket>, EncodeError> {
    let Some(buffer) = sample.buffer() else {
        return Ok(None);
    };
    let map = buffer
        .map_readable()
        .map_err(|e| backend("map buffer readable", e))?;
    let data = map.as_slice().to_vec();
    let pts_ns = buffer.pts().map(|t| t.nseconds()).unwrap_or(0);
    let dts_ns = buffer.dts().map(|t| t.nseconds());
    // Audio (AAC/Opus) frames are all independently decodable.
    let is_keyframe =
        track == Track::Audio || !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

    Ok(Some(EncodedPacket {
        data,
        pts_ns,
        dts_ns,
        is_keyframe,
        track,
    }))
}

fn try_set_u32(element: &gst::Element, name: &str, value: u32) -> bool {
    if element.find_property(name).is_none() {
        return false;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        element.set_property(name, value);
    }))
    .is_ok()
}

fn try_set_i32(element: &gst::Element, name: &str, value: i32) -> bool {
    if element.find_property(name).is_none() {
        return false;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        element.set_property(name, value);
    }))
    .is_ok()
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

pub struct GstMuxer;

impl Muxer for GstMuxer {
    fn write_clip(
        &self,
        video: &[EncodedPacket],
        _info: StreamInfo,
        audio: &[EncodedPacket],
        audio_info: Option<AudioInfo>,
        settings: &EncodeSettings,
        out: &std::path::Path,
    ) -> Result<(), EncodeError> {
        init()?;

        let (vparse, vmedia) = match settings.codec {
            Codec::H264 => ("h264parse", "video/x-h264"),
            Codec::Hevc => ("h265parse", "video/x-h265"),
        };
        let mux_name = match settings.container {
            Container::Mp4 => "mp4mux",
            Container::Mkv => "matroskamux",
        };

        let pipeline = gst::Pipeline::new();

        let mux = gst::ElementFactory::make(mux_name)
            .build()
            .map_err(|e| backend(&format!("make {mux_name}"), e))?;
        if settings.container == Container::Mp4 {
            let _ = try_set_bool(&mux, "faststart", true);
        }
        let filesink = gst::ElementFactory::make("filesink")
            .build()
            .map_err(|e| backend("make filesink", e))?;
        let location = out
            .to_str()
            .ok_or_else(|| EncodeError::Backend(format!("non-UTF-8 output path: {out:?}")))?;
        filesink.set_property("location", location);

        pipeline
            .add_many([&mux, &filesink])
            .map_err(|e| backend("mux add core", e))?;
        gst::Element::link_many([&mux, &filesink]).map_err(|e| backend("link mux->filesink", e))?;

        // --- video branch ----------------------------------------------------
        let v_caps = gst::Caps::builder(vmedia)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let vsrc = gst_app::AppSrc::builder()
            .caps(&v_caps)
            .is_live(false)
            .format(gst::Format::Time)
            .build();
        let vparse_el = gst::ElementFactory::make(vparse)
            .build()
            .map_err(|e| backend(&format!("make {vparse}"), e))?;
        pipeline
            .add_many([vsrc.upcast_ref::<gst::Element>(), &vparse_el])
            .map_err(|e| backend("mux add video", e))?;
        gst::Element::link_many([vsrc.upcast_ref::<gst::Element>(), &vparse_el])
            .map_err(|e| backend("link video src->parse", e))?;
        vparse_el
            .link(&mux)
            .map_err(|e| backend("link video parse->mux", e))?;

        // --- audio branch (optional) -----------------------------------------
        let asrc = if let Some(ai) = audio_info.filter(|_| !audio.is_empty()) {
            let (a_caps, a_parse) = match ai.codec {
                AudioCodec::Aac => (
                    gst::Caps::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "adts")
                        .build(),
                    "aacparse",
                ),
                AudioCodec::Opus => (gst::Caps::builder("audio/x-opus").build(), "opusparse"),
            };
            let asrc = gst_app::AppSrc::builder()
                .caps(&a_caps)
                .is_live(false)
                .format(gst::Format::Time)
                .build();
            let aparse_el = gst::ElementFactory::make(a_parse)
                .build()
                .map_err(|e| backend(&format!("make {a_parse}"), e))?;
            pipeline
                .add_many([asrc.upcast_ref::<gst::Element>(), &aparse_el])
                .map_err(|e| backend("mux add audio", e))?;
            gst::Element::link_many([asrc.upcast_ref::<gst::Element>(), &aparse_el])
                .map_err(|e| backend("link audio src->parse", e))?;
            aparse_el
                .link(&mux)
                .map_err(|e| backend("link audio parse->mux", e))?;
            Some(asrc)
        } else {
            None
        };

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| backend("set mux pipeline Playing", e))?;

        // Video and audio are captured on different clocks (rebased-monotonic vs
        // the live audio source's running-time). Rebase each track to its own 0
        // origin so the container has sane, aligned timestamps and a correct
        // duration — a mismatch here yields a bogus duration and a moov that some
        // demuxers reject.
        let vbase = video.iter().map(|p| p.pts_ns).min().unwrap_or(0);
        push_packets(&vsrc, video, vbase)?;
        vsrc.end_of_stream()
            .map_err(|e| backend("video eos", e))?;
        if let Some(asrc) = &asrc {
            let abase = audio.iter().map(|p| p.pts_ns).min().unwrap_or(0);
            push_packets(asrc, audio, abase)?;
            asrc.end_of_stream().map_err(|e| backend("audio eos", e))?;
        }

        wait_for_eos(&pipeline, 30)
    }
}

/// Push encoded packets into an appsrc as timestamped buffers, rebasing every
/// timestamp so the track starts at 0 (`base_ns` is subtracted).
fn push_packets(
    src: &gst_app::AppSrc,
    packets: &[EncodedPacket],
    base_ns: u64,
) -> Result<(), EncodeError> {
    for pkt in packets {
        let mut buffer = gst::Buffer::from_slice(pkt.data.clone());
        {
            let buf = buffer
                .get_mut()
                .ok_or_else(|| EncodeError::Backend("fresh mux buffer shared".into()))?;
            let pts = pkt.pts_ns.saturating_sub(base_ns);
            buf.set_pts(gst::ClockTime::from_nseconds(pts));
            // mp4mux needs a DTS; fall back to PTS when the encoder didn't set
            // one (our streams have no B-frames, so DTS == PTS).
            let dts = pkt.dts_ns.unwrap_or(pkt.pts_ns).saturating_sub(base_ns);
            buf.set_dts(gst::ClockTime::from_nseconds(dts));
            if !pkt.is_keyframe {
                buf.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        src.push_buffer(buffer)
            .map_err(|e| backend("mux push_buffer", e))?;
    }
    Ok(())
}

/// Run a pipeline's bus until EOS (success) or an error / timeout.
fn wait_for_eos(pipeline: &gst::Pipeline, secs: u64) -> Result<(), EncodeError> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| EncodeError::Backend("pipeline has no bus".into()))?;
    let mut result = Ok(());
    let mut saw_eos = false;
    for msg in bus.iter_timed(Some(gst::ClockTime::from_seconds(secs))) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => {
                saw_eos = true;
                break;
            }
            MessageView::Error(err) => {
                result = Err(EncodeError::Backend(format!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                )));
                break;
            }
            _ => {}
        }
    }
    if result.is_ok() && !saw_eos {
        result = Err(EncodeError::Backend("pipeline timed out before EOS".into()));
    }
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn try_set_bool(element: &gst::Element, name: &str, value: bool) -> bool {
    if element.find_property(name).is_none() {
        return false;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        element.set_property(name, value);
    }))
    .is_ok()
}

// ---------------------------------------------------------------------------
// Auto-convert (post-save transcode to shareable MP4)
// ---------------------------------------------------------------------------

/// Transcode `input` to a standard H.264/AAC MP4 with `faststart` at `output`.
pub fn convert_to_shareable(
    input: &std::path::Path,
    output: &std::path::Path,
    settings: &EncodeSettings,
) -> Result<(), EncodeError> {
    init()?;

    let inp = input
        .to_str()
        .ok_or_else(|| EncodeError::Backend(format!("non-UTF-8 input path: {input:?}")))?;
    let outp = output
        .to_str()
        .ok_or_else(|| EncodeError::Backend(format!("non-UTF-8 output path: {output:?}")))?;

    let vkbps = settings.bitrate_kbps.max(1_000);
    let abits = settings.audio_bitrate_kbps.max(96).saturating_mul(1000);

    // decodebin exposes dynamic pads; the launch parser links them to the
    // matching branch by caps. Audio branch is included only if we expect audio.
    let audio_branch = if settings.capture_audio {
        format!(
            " d. ! queue ! audioconvert ! audioresample ! avenc_aac bitrate={abits} ! aacparse ! m."
        )
    } else {
        String::new()
    };
    let desc = format!(
        "filesrc location=\"{inp}\" ! decodebin name=d \
         d. ! queue ! videoconvert ! x264enc bitrate={vkbps} speed-preset=veryfast key-int-max=60 \
         ! h264parse ! mp4mux name=m faststart=true ! filesink location=\"{outp}\"{audio_branch}"
    );

    let element = gst::parse::launch(&desc).map_err(|e| backend("parse convert pipeline", e))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| EncodeError::Backend("convert pipeline is not a Pipeline".into()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| backend("set convert pipeline Playing", e))?;
    wait_for_eos(&pipeline, 120)
}
