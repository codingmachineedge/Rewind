# Rewind

**A lightweight, privacy-respecting game clip recorder for Linux — an open-source alternative to NVIDIA ShadowPlay and the Xbox Game Bar.**

Rewind continuously records your gameplay into a rolling in-memory buffer and lets you save the **last N seconds** to disk with a single hotkey — so you never miss the moment. It is built to stay out of your way: no account, no telemetry, no background "assistant," and no upsells. Just capture, buffer, and save. It aims for minimal CPU/GPU overhead so it won't cost you frames, and everything it captures stays on your machine.

## Why Rewind?

Linux gamers have never had a great "instant replay" tool. The mainstream options are Windows-only, and the closest Linux equivalents mean hand-rolling OBS replay buffers or shell scripts. The proprietary recorders that do exist come with baggage: mandatory sign-ins, phone-home telemetry, ballooning background services, and nag screens. Rewind is deliberately *not predatory* — it does one job well, on Linux, and respects you while doing it.

## Planned Features

- **Background buffer capture** — continuously encode gameplay into a fixed-size ring buffer (configurable duration).
- **Save-last-N-seconds hotkey** — press a global hotkey to instantly flush the buffer to an `.mp4`/`.mkv` clip.
- **Low overhead** — hardware-accelerated capture and GPU (VA-API / NVENC) encoding to minimize FPS impact.
- **No telemetry, no account** — nothing is uploaded, nothing is tracked, no login required.
- **Local-first** — clips are written straight to a folder you choose; you own your data.
- **Wayland *and* X11** — first-class support for the modern Linux desktop, with a fallback for legacy sessions.
- **Native GUI** — a small GTK4 + libadwaita control window (start/stop, save, settings), plus a headless mode.
- **Configurable** — buffer length, output quality, hotkey, and save location via a simple config file.

## Linux capture stack

Rewind builds on the standard Linux screen-capture pipeline rather than reinventing it:

- **Wayland:** capture via the **PipeWire** + **xdg-desktop-portal** `ScreenCast` API (the same mechanism OBS and `wf-recorder` use). This is the sanctioned, compositor-agnostic path and works under wlroots (Sway, Hyprland), GNOME, and KDE.
- **X11:** capture via XComposite / XShm (or PipeWire where available) for legacy sessions.
- **Encode/mux:** a **GStreamer** pipeline (or direct VA-API / NVENC) handles hardware-accelerated H.264/HEVC encoding and muxing into `.mp4`/`.mkv`.
- **GUI:** **GTK4 + libadwaita** (via `gtk4-rs`), for a native GNOME/Linux look and feel.
- Conceptually similar to an OBS replay buffer, but headless-capable, single-purpose, and lightweight.

## Status

🚧 Early scaffold. The ring buffer and a wired-up GUI are in place; the capture/encode pipeline is stubbed — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the [wiki](../../wiki) for guides.

## Building

Requires [Rust](https://rustup.rs/) (stable) on Linux.

```sh
# Headless core (no system deps) — builds anywhere:
cargo build
cargo run

# Native GUI (needs GTK4 + libadwaita dev libraries + pkg-config on Linux):
cargo run --features gui
```

On Debian/Ubuntu the GUI build needs `libgtk-4-dev` and `libadwaita-1-dev`; on Fedora `gtk4-devel` and `libadwaita-devel`. Runtime capture will additionally depend on your session providing PipeWire + a desktop portal (Wayland) or an X11 server — see the wiki's **Getting Started** page.

## Contributing

Contributions are welcome. This is an open-source project (MIT licensed) — file an issue or open a PR. See the wiki's **Getting Started** page for a dev-environment walkthrough.

## License

[MIT](LICENSE) © Rewind contributors
