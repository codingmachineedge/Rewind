# Rewind Architecture

This document sketches the intended design of Rewind. It describes the
target pipeline; the current codebase is an early scaffold with the buffer API
implemented and the capture/encode stages stubbed.

**Target platform: Linux** (Wayland and X11 desktop sessions).

## Design goals

1. **Low overhead** — must not meaningfully cost the user frames while gaming.
   Prefer hardware capture and GPU-accelerated encoding; keep allocations off
   the hot path.
2. **Privacy-first** — no telemetry, no account, no network calls. All footage
   stays local until the user explicitly saves and shares it.
3. **Instant recall** — the "save last N seconds" action must be effectively
   instantaneous because the data is already buffered in memory.

## Pipeline overview

```
+---------------------+     +--------------+     +------------------+     +-----------+
|   Frame Capture     | --> |   Encoder    | --> |   Ring Buffer    | --> | Clip Muxer|
| PipeWire/portal(WL) |     | (VA-API /    |     | (last N seconds) |     | (.mp4/    |
| XComposite/XShm(X11)|     |  NVENC/GST)  |     |                  |     |  .mkv)    |
+---------------------+     +--------------+     +------------------+     +-----------+
        |                                               ^                      ^
        |                                               |                      |
     capture thread                             global hotkey ----------------+
```

## Wayland vs X11 capture

Linux has no single screen-capture API — the right path depends on the session:

### Wayland (preferred)
Wayland compositors deliberately deny arbitrary screen access. The sanctioned
route is the **`org.freedesktop.portal.ScreenCast`** interface of
**xdg-desktop-portal**, which hands back a **PipeWire** stream after the user
grants permission. This is the same mechanism OBS Studio and `wf-recorder` use,
and it works across wlroots (Sway, Hyprland), GNOME (Mutter), and KDE (KWin).

- Pros: compositor-agnostic, user-consented, zero-copy DMA-BUF frames.
- Considerations: capture requires an interactive permission grant; a
  `restore_token` is persisted so re-launches don't re-prompt. GNOME vs wlroots
  portals differ slightly in cursor/region options.
- Crates: `ashpd` (portal), `pipewire` (stream).

### X11 (legacy fallback)
For X11 sessions, capture directly with **XComposite + XShm** (shared-memory
image transfer) or use PipeWire where the portal is available. X11 imposes no
permission prompt, but offers no DMA-BUF fast path, so this is the higher-overhead
path.

- Crates: `x11rb` (or `xcb`).

The capture layer is abstracted behind a `FrameSource` trait so `main` is
agnostic to which backend (Wayland/PipeWire or X11) is active; selection happens
at runtime from `$XDG_SESSION_TYPE`.

### Capture target (monitor vs window)

`Config::capture_target` (a `CaptureTarget`) chooses what to grab, exposed as a
dropdown in the GUI:

- **Monitor** — the whole screen / X11 root window (default, original behavior).
- **Window** — a single window, re-attached across relaunches. On X11 the chosen
  window's `WM_CLASS`/title is persisted to `~/.config/rewind/window.target` and
  re-found via `_NET_CLIENT_LIST`; XComposite (`RedirectAutomatic` +
  `NameWindowPixmap`) lets occluded windows still capture. On Wayland the portal
  owns window selection and the per-target `restore_token` handles re-attach, so
  relaunches don't re-prompt (the origin-thread complaint about OBS).
- **Active window** — X11 resolves `_NET_ACTIVE_WINDOW` at capture start (and
  remembers it for Window mode); Wayland treats this as Window (no portal API for
  the active window).

## Encoder & muxer

A **GStreamer** pipeline handles hardware-accelerated H.264/HEVC encoding
(`vaapih264enc` / `nvh264enc`) and muxing into `.mp4`/`.mkv`. GStreamer also
bridges cleanly to the PipeWire source (`pipewiresrc`), keeping the whole path in
one framework where possible. Direct VA-API/NVENC remain an option for the
lowest-overhead builds.

## Ring buffer (`src/buffer.rs`)

A fixed-capacity ring of encoded frames sized to `buffer_seconds * target_fps`.
The oldest frame is overwritten once full — memory usage is bounded and
predictable. On the save hotkey, the buffered frames (ordered from the write
head) are muxed and written atomically to the user's output directory.
**This is the piece implemented today.**

## User interface

A native **GTK4 + libadwaita** GUI (`src/gui.rs`, via `gtk4-rs`) provides the
control surface: a start/stop capture toggle, a "Save last N seconds" button, a
settings group (buffer length, output folder, hotkey), and a status line — all
wired to `Config` and `ClipBuffer`. It's gated behind the `gui` cargo feature
(`cargo run --features gui`) so the headless core still builds without the GTK
system libraries. The `main.rs` CLI path remains as a headless fallback.

## Modules

| File                       | Feature            | Responsibility                                                        |
|----------------------------|--------------------|----------------------------------------------------------------------|
| `src/main.rs`              | —                  | Entry point; launches the GUI (`gui`) or a headless CLI runner.       |
| `src/media.rs`             | —                  | Shared types: `Frame`, `PixelFormat`, `StreamInfo`, `EncodedPacket`.  |
| `src/config.rs`            | —                  | Runtime config, local-first defaults, encode settings, clip naming.   |
| `src/buffer.rs`            | —                  | `ClipBuffer` — time-bounded ring of encoded packets, keyframe snapshot.|
| `src/pipeline.rs`          | —                  | Orchestrator: capture → encode worker → buffer → save; hotkey wiring. |
| `src/capture/mod.rs`       | —                  | `FrameSource` trait + `$XDG_SESSION_TYPE` backend selector.           |
| `src/capture/wayland.rs`   | `capture-wayland`  | PipeWire + portal ScreenCast; monitor/window `SourceType` + `restore_token`. |
| `src/capture/x11.rs`       | `capture-x11`      | XShm capture of the root or a single window (XComposite; re-attach by WM_CLASS). |
| `src/encode/mod.rs`        | —                  | `Encoder` + `Muxer` traits and backend selectors.                    |
| `src/encode/gstreamer.rs`  | `encode-gstreamer` | GStreamer hw encode (VA-API/NVENC/x264) + MP4/MKV mux.               |
| `src/hotkey/mod.rs`        | —                  | `HotkeyManager` trait + selector.                                    |
| `src/hotkey/portal.rs`     | `hotkey`           | Portal GlobalShortcuts, with an evdev fallback.                      |
| `src/gui.rs`               | `gui`              | GTK4 + libadwaita window wired to the live `Pipeline`.               |

The core (everything with no feature) is std-only and compiles on any host; when
a backend feature is off, its selector returns an "unsupported" error/`None` and
the pipeline degrades gracefully, so the default build stays green.

## Roadmap

- [x] `FrameSource` trait with Wayland (PipeWire/portal) + X11 backends
- [x] Portal `restore_token` persistence (no re-prompt on relaunch)
- [x] Hardware encoder integration (GStreamer / VA-API / NVENC / x264)
- [x] Global hotkey registration (portal GlobalShortcuts + evdev fallback)
- [x] MP4/MKV muxing on flush (keyframe-aligned from the ring buffer)
- [x] Continuous encode into a time-bounded ring buffer
- [x] Audio capture (PipeWire) muxed as a second track, A/V aligned
- [x] Auto-convert saved clips to a shareable H.264/AAC MP4 (faststart)
- [x] GUI wired to the live pipeline (start/stop, save, settings, status)
- [x] Runtime-verified end-to-end in a real Ubuntu GNOME session (X11 + audio)
- [x] Start-at-login via XDG autostart (`--install-autostart`)
- [ ] Verify the Wayland portal ScreenCast path end-to-end (needs the grant dialog)
- [ ] Verify hardware encode on a real GPU (VA-API/NVENC)
- [x] Per-window capture that re-attaches to the same window across relaunches
      (the X11 behavior people miss on Wayland), with a capture-active-window mode
- [ ] DMABUF fast path for zero-copy Wayland frames
- [ ] TOML config loading + first-run setup (persist GUI settings)
- [ ] Tray icon / background daemon mode

## Audio & A/V timing

Audio is captured inside the same GStreamer graph as video (`pulsesrc` from the
default sink's monitor → `audioconvert` → `audioresample` → AAC/Opus → parser →
appsink), producing `Track::Audio`-tagged packets that flow into a parallel ring
buffer. Video frames are stamped with an explicit, monotonic, 0-based PTS; audio
uses the live source's running-time. On save, the muxer **rebases each track to
its own 0 origin** before muxing, which keeps the container duration correct and
the moov well-formed (a mismatch here produces a bogus multi-hour duration and a
file some demuxers reject). Both tracks then start at 0 and stay aligned.
