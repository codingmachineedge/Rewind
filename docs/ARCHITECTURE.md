# ClipForge Architecture

This document sketches the intended design of ClipForge. It describes the
target pipeline; the current codebase is an early scaffold with the buffer API
implemented and the capture/encode stages stubbed.

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
+-----------------+     +--------------+     +------------------+     +-----------+
|  Frame Capture  | --> |   Encoder    | --> |   Ring Buffer    | --> | Clip Muxer|
| (WGC / DXGI)    |     | (HW H.264)   |     | (last N seconds) |     | (.mp4)    |
+-----------------+     +--------------+     +------------------+     +-----------+
        |                                            ^                      ^
        |                                            |                      |
     capture thread                          global hotkey ----------------+
```

### 1. Frame capture
Use the **Windows.Graphics.Capture (WGC)** API (fallback: DXGI Desktop
Duplication) to grab frames from the active display or a specific game window
with minimal copies. Runs on a dedicated capture thread.

### 2. Encoder
Feed captured surfaces to a hardware H.264/HEVC encoder (Media Foundation, or
NVENC/AMF/QuickSync where available). Encoding in real time keeps the buffer
compact so seconds of footage fit comfortably in RAM.

### 3. Ring buffer (`src/buffer.rs`)
A fixed-capacity ring of encoded frames sized to `buffer_seconds * target_fps`.
The oldest frame is overwritten once full — memory usage is bounded and
predictable. **This is the piece implemented today.**

### 4. Clip muxer
On the save hotkey, the buffered frames (ordered from the write head) are muxed
into an `.mp4`/`.mkv` and written atomically to the user's output directory.

## Modules

| File             | Responsibility                                            |
|------------------|-----------------------------------------------------------|
| `src/main.rs`    | Entry point; wires config + buffer, hosts the stub loop.  |
| `src/buffer.rs`  | `ClipBuffer` ring buffer + `EncodedFrame` (implemented).  |
| `src/config.rs`  | Runtime configuration and local-first defaults.           |

## Roadmap

- [ ] WGC capture thread feeding real frames into the buffer
- [ ] Hardware encoder integration
- [ ] Global hotkey registration
- [ ] MP4 muxing on flush
- [ ] TOML config loading + first-run setup
- [ ] Tray UI / minimal control surface
