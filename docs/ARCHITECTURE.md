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

| File             | Responsibility                                            |
|------------------|-----------------------------------------------------------|
| `src/main.rs`    | Entry point; launches the GUI (feature `gui`) or CLI stub.|
| `src/gui.rs`     | GTK4 + libadwaita control window (feature `gui`).         |
| `src/buffer.rs`  | `ClipBuffer` ring buffer + `EncodedFrame` (implemented).  |
| `src/config.rs`  | Runtime configuration and local-first defaults.           |

## Roadmap

- [ ] `FrameSource` trait with Wayland (PipeWire/portal) + X11 backends
- [ ] Portal `restore_token` persistence (no re-prompt on relaunch)
- [ ] Hardware encoder integration (GStreamer / VA-API / NVENC)
- [ ] Global hotkey registration (works under Wayland input constraints)
- [ ] MP4/MKV muxing on flush
- [ ] TOML config loading + first-run setup (persist GUI settings)
- [ ] Wire the GUI toggle to the real capture thread
- [ ] Tray icon / background daemon mode
