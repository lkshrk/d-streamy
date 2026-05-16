# D-Streamy

macOS menu bar app that streams any window to a Discord voice channel via Go Live (screenshare).

## Architecture

- **Swift app** — ScreenCaptureKit capture + VideoToolbox H.264 encoding
- **Node daemon** — Discord protocol via discord-video-stream, communicates over Unix socket + stdin pipe

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

The app auto-reconnects on disconnect and resumes streaming if moved to another channel.

## Disclaimer

Use at your own risk. This project uses a self-bot (user token automation) which violates Discord's Terms of Service. Your account may be suspended or banned. The authors take no responsibility for any consequences.
