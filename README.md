# ClipForge

**A lightweight, privacy-respecting game clip recorder for Windows — an open-source alternative to NVIDIA ShadowPlay and the Xbox Game Bar.**

ClipForge continuously records your gameplay into a rolling in-memory buffer and lets you save the **last N seconds** to disk with a single hotkey — so you never miss the moment. It is built to stay out of your way: no account, no telemetry, no background "assistant," and no upsells. Just capture, buffer, and save. It aims for minimal CPU/GPU overhead so it won't cost you frames, and everything it captures stays on your machine.

## Why ClipForge?

The mainstream clip recorders work, but they come with baggage: mandatory sign-ins, phone-home telemetry, ballooning background services, and nag screens. ClipForge is deliberately *not predatory* — it does one job well and respects you while doing it.

## Planned Features

- **Background buffer capture** — continuously encode gameplay into a fixed-size ring buffer (configurable duration).
- **Save-last-N-seconds hotkey** — press a global hotkey to instantly flush the buffer to an `.mp4` clip.
- **Low overhead** — hardware-accelerated capture (Windows.Graphics.Capture / DXGI) and GPU encoding to minimize FPS impact.
- **No telemetry, no account** — nothing is uploaded, nothing is tracked, no login required.
- **Local-first** — clips are written straight to a folder you choose; you own your data.
- **Configurable** — buffer length, output quality, hotkey, and save location via a simple config file.

## Status

🚧 Early scaffold. The buffer/capture pipeline is stubbed out — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the [wiki](../../wiki) for guides.

## Building

Requires [Rust](https://rustup.rs/) (stable) on Windows.

```sh
cargo build --release
cargo run
```

## Contributing

Contributions are welcome. This is an open-source project (MIT licensed) — file an issue or open a PR. See the wiki's **Getting Started** page for a dev-environment walkthrough.

## License

[MIT](LICENSE) © ClipForge contributors
