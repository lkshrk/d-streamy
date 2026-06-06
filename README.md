# D-Streamy

macOS menu bar app that streams any window to a Discord voice channel via Go Live (screenshare).

## Architecture

- **Swift app** — ScreenCaptureKit capture + VideoToolbox hardware encoding (H.264 or H.265)
- **Node daemon** — Discord protocol via discord-video-stream, communicates over Unix socket + stdin pipe

## Features

- **Window or display capture** with optional live crop overlay
- **Configurable** resolution (up to 1080p), FPS (15/30/60), bitrate, audio gain
- **Codec toggle** — H.264 (default, universal) or H.265/HEVC (~30–50% bitrate savings). **Note: Discord does not currently support H.265 for viewing in practice** — its SFU forwards without transcoding, and most desktop/web clients can't decode incoming HEVC (receive is largely mobile-only or gated behind a client experiment flag), so H.265 viewers typically see no video. The encoder path is correct; the limitation is Discord/viewer-side. Leave this on H.264 unless you know every viewer can decode HEVC.
- **Live stream-health monitoring** in the menu bar window — per-stream Video/Audio stability score + sparkline, color-coded metrics (FPS, frame jitter, drops, audio cadence); the menu bar icon dot turns green/yellow/red by health and blinks when unstable
- **Auto-reconnect** on disconnect; resumes streaming when moved to another channel

## Requirements

- macOS 14+
- [Bun](https://bun.sh) runtime
- Discord user token

## Build

```bash
bun install
bash scripts/build.sh
```

Output: `bin/D-Streamy.app`

## Usage

1. Launch app (appears in menu bar)
2. Set Discord token in settings
3. Select server and voice channel
4. Pick a window to stream
5. Click Start

## Disclaimer

Use at your own risk. This project uses a self-bot (user token automation) which violates Discord's Terms of Service. Your account may be suspended or banned. The authors take no responsibility for any consequences.
