//! GStreamer encode/mux backend (feature `encode-gstreamer`, Linux only).
//!
//! Two pieces implement the [`Encoder`] and [`Muxer`] traits from
//! [`crate::encode`]:
//!
//! * [`GstEncoder`] runs a live, continuous pipeline
//!   `appsrc ! videoconvert ! <hwenc|x264enc> ! h264parse|h265parse ! appsink`.
//!   Raw [`Frame`]s are pushed into `appsrc`; encoded access units are pulled
//!   out of `appsink` as [`EncodedPacket`]s.
//! * [`GstMuxer`] builds a one-shot pipeline
//!   `appsrc ! h264parse|h265parse ! <mp4mux|matroskamux> ! filesink` to write
//!   a buffered clip to disk.
//!
//! Hardware encoders are selected at runtime by probing the GStreamer registry
//! with [`gst::ElementFactory::find`], preferring VA-API, then NVENC, then a
//! software fallback (`x264enc` / `x265enc`).
//!
//! NOTE: This module targets `gstreamer-rs` 0.23 and can only be *compiled and
//! verified on a Linux host* with GStreamer + plugins installed. All `// NOTE:`
//! comments below flag 0.23 API surface that should be double-checked against a
//! real build (exact builder shapes, `try_pull_sample` signature, caps forms,
//! element availability on the target machine).

use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::encode::{EncodeError, Encoder, Muxer};
use crate::media::{Codec, Container, EncodeSettings, EncodedPacket, Frame, StreamInfo};

/// Initialize GStreamer. Idempotent: safe to call from every entry point.
fn init() -> Result<(), EncodeError> {
    gst::init().map_err(|e| EncodeError::Backend(format!("gst::init failed: {e}")))
}

/// Map an arbitrary error (usually `glib::Error` / `BoolError`) to a backend error.
fn backend<E: std::fmt::Display>(ctx: &str, e: E) -> EncodeError {
    EncodeError::Backend(format!("{ctx}: {e}"))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Construct the continuous GStreamer encoder.
pub fn encoder() -> Result<Box<dyn Encoder>, EncodeError> {
    init()?;
    Ok(Box::new(GstEncoder::new()))
}

/// Construct the GStreamer muxer.
pub fn muxer() -> Result<Box<dyn Muxer>, EncodeError> {
    init()?;
    Ok(Box::new(GstMuxer::new()))
}

// ---------------------------------------------------------------------------
// Encoder-element selection
// ---------------------------------------------------------------------------

/// A chosen encoder element factory name plus the parser that follows it.
struct EncoderChoice {
    /// e.g. `"vaapih264enc"`, `"nvh264enc"`, `"x264enc"`.
    enc: &'static str,
    /// `"h264parse"` or `"h265parse"`.
    parse: &'static str,
    /// The encoded-caps media type, e.g. `"video/x-h264"`.
    media_type: &'static str,
    /// Whether the chosen encoder is the software fallback (needs tune/key-int).
    is_software: bool,
}

/// Return true if a GStreamer element factory of this name is registered.
fn have_element(name: &str) -> bool {
    // NOTE: 0.23 — `ElementFactory::find` returns `Option<ElementFactory>`.
    gst::ElementFactory::find(name).is_some()
}

/// Pick an encoder element for the requested codec, in priority order.
/// H264: vaapih264enc -> nvh264enc -> x264enc.
/// HEVC: vaapih265enc -> nvh265enc -> x265enc.
fn choose_encoder(codec: Codec) -> Result<EncoderChoice, EncodeError> {
    let (candidates, parse, media_type, software): (
        &[&'static str],
        &'static str,
        &'static str,
        &'static str,
    ) = match codec {
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

    for &name in candidates {
        if have_element(name) {
            return Ok(EncoderChoice {
                enc: name,
                parse,
                media_type,
                is_software: name == software,
            });
        }
    }

    Err(EncodeError::Unsupported(format!(
        "no GStreamer encoder available for {codec:?} (tried {candidates:?})"
    )))
}

// ---------------------------------------------------------------------------
// Continuous encoder
// ---------------------------------------------------------------------------

/// Live encoding pipeline. All GStreamer handles live behind a `Mutex` so the
/// struct is `Send` regardless of the internal thread-affinity of the objects.
pub struct GstEncoder {
    inner: Mutex<Option<EncoderInner>>,
    name: String,
}

struct EncoderInner {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    appsink: gst_app::AppSink,
    eos_sent: bool,
}

impl GstEncoder {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
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

        // --- appsrc -----------------------------------------------------------
        // Raw video caps derived from the negotiated stream info.
        // NOTE: 0.23 — `gst::Caps::builder("video/x-raw").field(...).build()`.
        // Framerate is a `gst::Fraction` (num/den).
        let src_caps = gst::Caps::builder("video/x-raw")
            .field("format", info.format.gst_format())
            .field("width", info.width as i32)
            .field("height", info.height as i32)
            .field("framerate", gst::Fraction::new(info.framerate as i32, 1))
            .build();

        // NOTE: 0.23 — `AppSrc::builder()` exists; `is_live`, `format`, `caps`
        // are builder setters. Alternatively build via ElementFactory and set
        // properties. Format::Time makes PTS meaningful.
        let appsrc = gst_app::AppSrc::builder()
            .caps(&src_caps)
            .is_live(true)
            .format(gst::Format::Time)
            .build();
        // Reduce internal blocking / latency for a live source.
        appsrc.set_do_timestamp(false);

        // --- videoconvert -----------------------------------------------------
        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| backend("make videoconvert", e))?;

        // --- encoder ----------------------------------------------------------
        let encoder = gst::ElementFactory::make(choice.enc)
            .build()
            .map_err(|e| backend(&format!("make {}", choice.enc), e))?;

        // Bitrate: property units differ per element. x264enc/x265enc take
        // kbit/s (matching `bitrate_kbps`). VA-API/NVENC also generally accept a
        // `bitrate` property in kbit/s. We set kbps directly.
        // NOTE: property name is "bitrate" on all four; value type is u32/i32
        // depending on element — `set_property` with a `u32` is accepted by
        // x264enc/x265enc; VA-API uses u32 kbps, NVENC uses u32 kbps as well.
        // If a build rejects the type, wrap with `i32`. Guarded with try.
        let _ = try_set_u32(&encoder, "bitrate", settings.bitrate_kbps);

        if choice.is_software {
            // x264enc / x265enc software fallback tuning.
            // tune=zerolatency (x264enc: flags enum; commonly settable by string
            // via `set_property_from_str`). key-int-max = keyframe interval.
            // NOTE: 0.23 — `set_property_from_str` sets enum/flags props from a
            // string. "zerolatency" is a valid x264enc tune flag. x265enc uses a
            // different tuning surface; the string set is best-effort here.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                encoder.set_property_from_str("tune", "zerolatency");
            }));
            let _ = try_set_u32(&encoder, "key-int-max", settings.keyframe_interval);
        } else {
            // Hardware encoders: keyframe cadence property is commonly named
            // differently ("keyframe-period" / "gop-size" / "key-int-max").
            // Best-effort across vendors.
            let _ = try_set_u32(&encoder, "key-int-max", settings.keyframe_interval);
            let _ = try_set_u32(&encoder, "keyframe-period", settings.keyframe_interval);
            let _ = try_set_u32(&encoder, "gop-size", settings.keyframe_interval);
        }

        // --- parser -----------------------------------------------------------
        let parser = gst::ElementFactory::make(choice.parse)
            .build()
            .map_err(|e| backend(&format!("make {}", choice.parse), e))?;

        // --- appsink ----------------------------------------------------------
        // Constrain to byte-stream / au so downstream muxing and keyframe flags
        // behave predictably.
        let sink_caps = gst::Caps::builder(choice.media_type)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();

        let appsink = gst_app::AppSink::builder()
            .caps(&sink_caps)
            .build();
        // We pull manually in `poll`/`flush`; `emit-signals` defaults to false,
        // so there is nothing to disable here.
        // Don't let the sink block the streaming thread indefinitely.
        appsink.set_max_buffers(0); // 0 == unlimited queue depth in appsink.
        appsink.set_drop(false);

        // --- assemble ---------------------------------------------------------
        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &videoconvert,
                &encoder,
                &parser,
                appsink.upcast_ref::<gst::Element>(),
            ])
            .map_err(|e| backend("pipeline add_many", e))?;

        gst::Element::link_many([
            appsrc.upcast_ref::<gst::Element>(),
            &videoconvert,
            &encoder,
            &parser,
            appsink.upcast_ref::<gst::Element>(),
        ])
        .map_err(|e| backend("link encode chain", e))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| backend("set encode pipeline Playing", e))?;

        let mut guard = self.inner.lock().unwrap();
        *guard = Some(EncoderInner {
            pipeline,
            appsrc,
            appsink,
            eos_sent: false,
        });
        self.name = format!("gstreamer:{}", choice.enc);
        Ok(())
    }

    fn push_frame(&mut self, frame: &Frame) -> Result<(), EncodeError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("push_frame before configure".into()))?;

        // Copy the frame bytes into a fresh GStreamer buffer.
        // NOTE: 0.23 — `gst::Buffer::from_slice(impl AsRef<[u8]>)` copies data
        // into a new buffer. `frame.data` is `Arc<Vec<u8>>`; deref to `&[u8]`.
        let bytes: &[u8] = frame.data.as_slice();
        let mut buffer = gst::Buffer::from_slice(bytes.to_vec());
        {
            let buf_ref = buffer.get_mut().ok_or_else(|| {
                EncodeError::Backend("fresh buffer unexpectedly shared".into())
            })?;
            // ClockTime is nanoseconds. pts_ns is a monotonic ns timestamp.
            buf_ref.set_pts(gst::ClockTime::from_nseconds(frame.pts_ns));
        }

        // NOTE: 0.23 — `AppSrc::push_buffer` returns `Result<FlowSuccess, FlowError>`.
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
        drain(&inner.appsink, gst::ClockTime::ZERO, sink)
    }

    fn flush(&mut self, sink: &mut dyn FnMut(EncodedPacket)) -> Result<(), EncodeError> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| EncodeError::Backend("flush before configure".into()))?;

        if !inner.eos_sent {
            // NOTE: 0.23 — `AppSrc::end_of_stream` -> Result<FlowSuccess, FlowError>.
            inner
                .appsrc
                .end_of_stream()
                .map_err(|e| backend("appsrc end_of_stream", e))?;
            inner.eos_sent = true;
        }

        // Drain remaining samples. `try_pull_sample` with a small timeout returns
        // None once the sink is EOS and empty.
        // NOTE: 0.23 — `AppSink::try_pull_sample(Option<ClockTime>)` returns
        // `Option<Sample>`. On EOS with an empty queue it returns None.
        let timeout = gst::ClockTime::from_mseconds(100);
        while let Some(sample) = inner.appsink.try_pull_sample(Some(timeout)) {
            if let Some(pkt) = sample_to_packet(&sample)? {
                sink(pkt);
            }
        }

        // Tear the pipeline down so a subsequent configure starts clean.
        inner
            .pipeline
            .set_state(gst::State::Null)
            .map_err(|e| backend("set encode pipeline Null", e))?;
        Ok(())
    }
}

/// Non-blocking / bounded drain of an appsink into `sink`.
fn drain(
    appsink: &gst_app::AppSink,
    timeout: gst::ClockTime,
    sink: &mut dyn FnMut(EncodedPacket),
) -> Result<(), EncodeError> {
    // ClockTime::ZERO => return immediately if nothing is queued.
    while let Some(sample) = appsink.try_pull_sample(Some(timeout)) {
        if let Some(pkt) = sample_to_packet(&sample)? {
            sink(pkt);
        }
    }
    Ok(())
}

/// Convert a pulled [`gst::Sample`] into an [`EncodedPacket`].
///
/// `is_keyframe` is the inverse of the buffer's `DELTA_UNIT` flag: a delta unit
/// depends on prior frames, so its absence marks a keyframe.
fn sample_to_packet(sample: &gst::Sample) -> Result<Option<EncodedPacket>, EncodeError> {
    let buffer = match sample.buffer() {
        Some(b) => b,
        None => return Ok(None),
    };

    // NOTE: 0.23 — `Buffer::map_readable()` -> Result<MapReadable, BoolError>;
    // `.as_slice()` yields `&[u8]`.
    let map = buffer
        .map_readable()
        .map_err(|e| backend("map buffer readable", e))?;
    let data = map.as_slice().to_vec();

    let pts_ns = buffer.pts().map(|t| t.nseconds()).unwrap_or(0);
    let dts_ns = buffer.dts().map(|t| t.nseconds());

    // A buffer WITHOUT the DELTA_UNIT flag is a keyframe.
    let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

    Ok(Some(EncodedPacket {
        data,
        pts_ns,
        dts_ns,
        is_keyframe,
    }))
}

/// Set a `u32` property if the element has it, swallowing type/absence errors.
///
/// GStreamer property setters panic on an unknown property or type mismatch, so
/// we guard with `catch_unwind` to make cross-vendor property probing safe.
fn try_set_u32(element: &gst::Element, name: &str, value: u32) -> bool {
    // Only attempt if the property actually exists to avoid a guaranteed panic.
    if element.find_property(name).is_none() {
        return false;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // NOTE: 0.23 — `ObjectExt::set_property(name, value)`. Value type must
        // match the pspec; some encoders declare `bitrate` as i32/i64. We try
        // u32 first; on a type mismatch the panic is caught and we return false.
        element.set_property(name, value);
    }))
    .is_ok()
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

/// One-shot muxer: writes buffered packets to a container file.
pub struct GstMuxer;

impl GstMuxer {
    fn new() -> Self {
        GstMuxer
    }
}

impl Muxer for GstMuxer {
    fn write_clip(
        &self,
        packets: &[EncodedPacket],
        _info: StreamInfo,
        settings: &EncodeSettings,
        out: &std::path::Path,
    ) -> Result<(), EncodeError> {
        init()?;

        let (parse, media_type) = match settings.codec {
            Codec::H264 => ("h264parse", "video/x-h264"),
            Codec::Hevc => ("h265parse", "video/x-h265"),
        };
        let mux = match settings.container {
            Container::Mp4 => "mp4mux",
            Container::Mkv => "matroskamux",
        };

        let pipeline = gst::Pipeline::new();

        // Encoded-stream caps; h264parse/h265parse will (re)timestamp and
        // convert to whatever alignment the muxer needs.
        let src_caps = gst::Caps::builder(media_type)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();

        let appsrc = gst_app::AppSrc::builder()
            .caps(&src_caps)
            .is_live(false)
            .format(gst::Format::Time)
            .build();

        let parser = gst::ElementFactory::make(parse)
            .build()
            .map_err(|e| backend(&format!("make {parse}"), e))?;

        let muxer = gst::ElementFactory::make(mux)
            .build()
            .map_err(|e| backend(&format!("make {mux}"), e))?;

        let filesink = gst::ElementFactory::make("filesink")
            .build()
            .map_err(|e| backend("make filesink", e))?;
        // NOTE: 0.23 — filesink "location" is a string property. Path -> str;
        // non-UTF-8 paths are rejected here (callers use UTF-8 clip paths).
        let location = out
            .to_str()
            .ok_or_else(|| EncodeError::Backend(format!("non-UTF-8 output path: {out:?}")))?;
        filesink.set_property("location", location);

        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &parser,
                &muxer,
                &filesink,
            ])
            .map_err(|e| backend("mux pipeline add_many", e))?;

        gst::Element::link_many([
            appsrc.upcast_ref::<gst::Element>(),
            &parser,
            &muxer,
            &filesink,
        ])
        .map_err(|e| backend("link mux chain", e))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| backend("set mux pipeline Playing", e))?;

        // Push every packet as a timestamped buffer.
        for pkt in packets {
            let mut buffer = gst::Buffer::from_slice(pkt.data.clone());
            {
                let buf_ref = buffer
                    .get_mut()
                    .ok_or_else(|| EncodeError::Backend("fresh mux buffer shared".into()))?;
                buf_ref.set_pts(gst::ClockTime::from_nseconds(pkt.pts_ns));
                if let Some(dts) = pkt.dts_ns {
                    buf_ref.set_dts(gst::ClockTime::from_nseconds(dts));
                }
                if !pkt.is_keyframe {
                    // Mark delta units so the parser/muxer treat non-keyframes
                    // correctly (keyframes carry no DELTA_UNIT flag).
                    buf_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
                }
            }
            appsrc
                .push_buffer(buffer)
                .map_err(|e| backend("mux appsrc push_buffer", e))?;
        }

        // Signal end of stream and wait for the muxer to finalize the file.
        appsrc
            .end_of_stream()
            .map_err(|e| backend("mux appsrc end_of_stream", e))?;

        // Run the bus loop until EOS or error.
        let bus = pipeline
            .bus()
            .ok_or_else(|| EncodeError::Backend("mux pipeline has no bus".into()))?;

        let mut result = Ok(());
        let mut saw_eos = false;
        // NOTE: 0.23 — `Bus::iter_timed(Option<ClockTime>)` yields messages.
        // `ClockTime::NONE` blocks indefinitely; we use a generous per-message
        // timeout so a stalled pipeline eventually ends the iterator (yields
        // None) rather than hanging forever.
        for msg in bus.iter_timed(Some(gst::ClockTime::from_seconds(30))) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => {
                    saw_eos = true;
                    break;
                }
                MessageView::Error(err) => {
                    result = Err(EncodeError::Backend(format!(
                        "mux pipeline error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    )));
                    break;
                }
                _ => {}
            }
        }

        // If the loop ended (per-message timeout with no more messages) without
        // an explicit EOS or error, the file is likely incomplete.
        if result.is_ok() && !saw_eos {
            result = Err(EncodeError::Backend(
                "mux pipeline timed out before EOS".into(),
            ));
        }

        // Always attempt to return the pipeline to Null.
        let _ = pipeline.set_state(gst::State::Null);

        result
    }
}
