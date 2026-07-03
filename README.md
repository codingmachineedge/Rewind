# Rewind

**A lightweight, privacy-respecting game clip recorder for Linux — an open-source alternative to NVIDIA ShadowPlay and the Xbox Game Bar.**

Rewind continuously records your gameplay into a rolling in-memory buffer and lets you save the **last N seconds** to disk with a single hotkey — so you never miss the moment. It is built to stay out of your way: no account, no telemetry, no background "assistant," and no upsells. Just capture, buffer, and save. It aims for minimal CPU/GPU overhead so it won't cost you frames, and everything it captures stays on your machine.

## Why Rewind?

Linux gamers have never had a great "instant replay" tool. The mainstream options are Windows-only, and the closest Linux equivalents mean hand-rolling OBS replay buffers or shell scripts. The proprietary recorders that do exist come with baggage: mandatory sign-ins, phone-home telemetry, ballooning background services, and nag screens. Rewind is deliberately *not predatory* — it does one job well, on Linux, and respects you while doing it.

## Planned Features

- **Background buffer capture** — continuously encode gameplay into a fixed-size ring buffer (configurable duration).
- **Save-last-N-seconds hotkey** — press a global hotkey to instantly flush the buffer to an `.mp4`/`.mkv` clip.
- **Audio too** — captures system/game audio via PipeWire and muxes it alongside the video, A/V aligned.
- **Auto-convert to shareable** — after each save, transcodes the clip to a standard H.264/AAC MP4 (faststart) in the background.
- **Low overhead** — hardware-accelerated capture and GPU (VA-API / NVENC) encoding to minimize FPS impact.
- **No telemetry, no account** — nothing is uploaded, nothing is tracked, no login required.
- **Local-first** — clips are written straight to a folder you choose; you own your data.
- **Wayland *and* X11** — first-class support for the modern Linux desktop, with a fallback for legacy sessions.
- **Native GUI** — a small GTK4 + libadwaita control window (start/stop, save, settings), plus a headless mode.
- **Configurable** — buffer length, output quality, hotkey, audio, and save location.

## Linux capture stack

Rewind builds on the standard Linux screen-capture pipeline rather than reinventing it:

- **Wayland:** capture via the **PipeWire** + **xdg-desktop-portal** `ScreenCast` API (the same mechanism OBS and `wf-recorder` use). This is the sanctioned, compositor-agnostic path and works under wlroots (Sway, Hyprland), GNOME, and KDE.
- **X11:** capture via XComposite / XShm (or PipeWire where available) for legacy sessions.
- **Encode/mux:** a **GStreamer** pipeline (or direct VA-API / NVENC) handles hardware-accelerated H.264/HEVC encoding and muxing into `.mp4`/`.mkv`.
- **Audio:** captured via PipeWire (`pulsesrc`) from the default sink's monitor, encoded to AAC/Opus, and muxed as a second track in the same GStreamer graph.
- **Share:** a post-save `decodebin → x264enc + AAC → mp4mux(faststart)` transcode produces a universally-playable copy.
- **GUI:** **GTK4 + libadwaita** (via `gtk4-rs`), for a native GNOME/Linux look and feel.
- Conceptually similar to an OBS replay buffer, but headless-capable, single-purpose, and lightweight.

## Requirements

Rewind will **not** require nuclear power, a spare GPU farm, or a slice of chocolate cake to run. 🍰⚡ It's a lightweight little thing — Rust, a screen, and your existing desktop are all it asks for. No absurd dependencies, no ritual sacrifices.

## Status

🚧 Under construction, but **real and runtime-verified**. The full pipeline works end-to-end: a `FrameSource` capture abstraction (Wayland via PipeWire/portal, X11 via XShm), continuous GStreamer encode into a time-bounded ring buffer, **PipeWire audio** muxed alongside, keyframe-aligned save-to-clip, **auto-convert** to a shareable MP4, an **evdev/portal global hotkey**, and a GTK4 GUI wired to the live pipeline.

Verified on a real Ubuntu 24.04 GNOME/Xorg session (software x264 encode): a 6-second capture produced a valid **H.264 + AAC** MP4 (correct duration, both streams) and an auto-converted shareable copy that plays. Hardware encode (VA-API/NVENC) is wired but needs a real GPU to exercise. The Linux backends build behind cargo features; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and the [wiki](../../wiki).

## Building

Requires [Rust](https://rustup.rs/) (stable). The headless core builds anywhere; the Linux backends need their system libraries.

```sh
# Headless core (no system deps) — builds on any platform:
cargo build
cargo run

# The full native Linux app (GUI + capture + encode + hotkeys):
cargo run --features linux

# Or pick individual slices:
cargo run --features gui                 # just the GTK window
cargo build --features capture-wayland   # Wayland/PipeWire capture
cargo build --features encode-gstreamer  # GStreamer encode/mux
```

**System packages** for `--features linux` (Debian/Ubuntu):

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev \
  libpipewire-0.3-dev pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-vaapi \
  libxcb-shm0-dev libxcb-composite0-dev libevdev-dev
```

On Fedora the equivalents are `gtk4-devel`, `libadwaita-devel`, `pipewire-devel`, `gstreamer1-devel`, `gstreamer1-plugins-{good,bad-free}`, etc. Runtime capture also needs your session to provide PipeWire + a desktop portal (Wayland) or an X11 server — see the wiki's **Getting Started** page.

## Contributing

Contributions are welcome. This is an open-source project (MIT licensed) — file an issue or open a PR. See the wiki's **Getting Started** page for a dev-environment walkthrough.

## License

[MIT](LICENSE) © Rewind contributors
