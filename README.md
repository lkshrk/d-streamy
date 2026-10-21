# D-Streamy

macOS menu bar app that streams any window to a Discord voice channel via Go Live (screenshare).

## Architecture

- **Swift app** (`App/`): the menu bar UI only. It starts the bundled `dstreamy daemon` and talks to it over a line-delimited JSON protocol on stdin/stdout ([ADR 0003](docs/adr/0003-macos-menu-bar-on-rust-daemon.md)).
- **`dstreamy`** (`rust/`): ScreenCaptureKit capture, VideoToolbox encoding (H.264 or H.265) and the Discord protocol. The same binary is the CLI below.

## Features

- **Window or display capture** with optional live crop overlay (windows)
- **Configurable** resolution (up to 1080p), FPS (15/30/60), bitrate, audio gain
- **Codec toggle** — H.264 (default, universal) or H.265/HEVC (~30–50% bitrate savings). **Note: Discord does not currently support H.265 for viewing in practice** — its SFU forwards without transcoding, and most desktop/web clients can't decode incoming HEVC (receive is largely mobile-only or gated behind a client experiment flag), so H.265 viewers typically see no video. The encoder path is correct; the limitation is Discord/viewer-side. Leave this on H.264 unless you know every viewer can decode HEVC.
- **Live stream-health monitoring** in the menu bar window — per-stream Video/Audio stability score + sparkline, color-coded metrics (FPS, frame jitter, drops, audio cadence); the menu bar icon dot turns green/yellow/red by health and blinks when unstable
- **Auto-reconnect** on disconnect; resumes streaming when moved to another channel

## Requirements

- macOS 14+
- Discord user token
- Screen Recording permission for D-Streamy (asked on the first stream)

## Build

Needs Xcode 26 and a Rust toolchain (see [Build from source](#build-from-source)):

```bash
bash scripts/build.sh
```

Output: `bin/D-Streamy.app`, with `dstreamy` in `Contents/MacOS/`. `DSTREAMY_DAEMON=<path>` bundles a prebuilt `dstreamy` instead of building one. When the app runs outside a bundle (`swift run`), it uses `DSTREAMY_BIN` or else `rust/target/debug/dstreamy`.

## Usage

1. Launch app (appears in menu bar)
2. Click Login and paste your Discord token (a token from an older version is moved over automatically)
3. Select server and voice channel
4. Pick a window or display to stream
5. Click Start

## dstreamy CLI (Rust rewrite, preview)

`dstreamy` is a single binary for macOS and Linux. On macOS it is also the engine behind the menu bar app. It lives in `rust/` and is released on its own from `rs-v*` tags.

### Install

Download `dstreamy-<version>-<target>.tar.gz` for your platform from the GitHub release, check it against the `.sha256` file next to it (`sha256sum -c` works too), and put `dstreamy` on your `PATH`:

```bash
shasum -a 256 -c dstreamy-0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf dstreamy-0.1.0-aarch64-apple-darwin.tar.gz
install -m 755 dstreamy-0.1.0-aarch64-apple-darwin/dstreamy ~/.local/bin/
```

Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. A tarball downloaded with a browser on macOS is quarantined; clear it with `xattr -d com.apple.quarantine ~/.local/bin/dstreamy`.

Requirements:

- macOS 14+ with Screen Recording permission for the terminal that runs `dstreamy`.
- Linux with glibc 2.39+ (Debian 13, Ubuntu 24.04 or newer), PipeWire, a D-Bus session and xdg-desktop-portal with a ScreenCast backend (GNOME, KDE or wlroots), plus GStreamer:

  ```bash
  sudo apt-get install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-pipewire gstreamer1.0-pulseaudio
  ```

  Hardware encoding is used when available: VA-API needs a VA driver for your GPU (and `gstreamer1.0-vaapi` for the older elements), NVENC needs the NVIDIA driver. Otherwise `x264enc` encodes in software.

### Build from source

macOS needs Xcode 26 (its SDK is required by `screencapturekit`) and a Rust toolchain:

```bash
cd rust
cargo build --release -p dstreamy-cli
./target/release/dstreamy --version
```

Linux additionally needs the GStreamer and PipeWire development packages:

```bash
sudo apt-get install clang libclang-dev pkg-config libpipewire-0.3-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libopus-dev
cd rust
cargo build --release -p dstreamy-cli
```

Or build the Linux binary in Docker from any host (the output lands in the `dstreamy-linux-target` volume, not in `rust/target`):

```bash
rust/ci/linux.sh cargo build --release -p dstreamy-cli
```

### Usage

```bash
dstreamy login                      # paste the Discord user token; it is checked, then stored
dstreamy guilds                     # list servers
dstreamy channels <GUILD>           # list voice channels of a server
dstreamy targets                    # list capturable displays and windows
dstreamy stream --guild <GUILD> --channel <CHANNEL> --target display:1
```

`DSTREAMY_TOKEN` overrides the stored token. `stream` runs until Ctrl-C and takes `--codec h264|h265` (h265 is experimental: most Discord viewers can't decode it), `--fps 15|30|60`, `--res 720p|900p|1080p|WIDTHxHEIGHT`, `--bitrate <kbps>`, `--gain <0.0-3.0>` and `--crop X,Y,W,H`; `--target test-pattern` streams a test pattern without capturing anything. Add `--json` for machine-readable output and `-v`/`-vv` for logs. On Linux, use `--target portal` and pick the window or screen in the portal dialog. See `dstreamy <command> --help` for details.

## Disclaimer

Use at your own risk. This project uses a self-bot (user token automation) which violates Discord's Terms of Service. Your account may be suspended or banned. The authors take no responsibility for any consequences.
