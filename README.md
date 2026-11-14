# D-Streamy

Streams a window, a display or OBS's output to a Discord voice channel through Go Live (screenshare).

- **Command line and TUI** on Linux and macOS: capture a window or display and stream it.
- **OBS plugin** on Linux, macOS and Windows: stream OBS's output to Discord from a panel in OBS's status bar, next to your normal Twitch or YouTube stream.
- **Menu bar app** on macOS.

All of them run on one binary, `dstreamy`. How it works is in [the documentation](docs/README.md).

> [!WARNING]
> D-Streamy automates a Discord user account, which breaks Discord's Terms of Service. Read the [disclaimer](#disclaimer) before using it.

## Features

- Window or display capture with an optional crop; on Linux you pick the window or screen in the desktop's sharing dialog.
- OBS's own output as the source, with its own encoders, so your normal stream is untouched.
- Up to 1080p at 15, 30 or 60 fps, with configurable bitrate, audio gain and audio device.
- Several saved Discord accounts; two accounts can stream at once.
- Stream health: video and audio scores, drops, reconnects and viewers.
- Automatic reconnects, and the stream follows when you are moved to another voice channel.
- DAVE end-to-end encryption when Discord offers it, with an option to refuse streaming without it.

H.264 is the default codec. H.265 exists for native capture but is experimental: most Discord viewers can't decode it.

## Install

No release of this version has been published yet. Until then, [build from source](#build-from-source). Each release attaches these files, each with a `.sha256` checksum:

| File | What |
|---|---|
| `dstreamy-<version>-<target>.tar.gz` | The CLI for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` |
| `dstreamy-<version>-x86_64-pc-windows-msvc.zip` | The CLI for Windows |
| `obs-dstreamy-<version>-linux-x86_64.tar.gz` | The OBS plugin for Linux |
| `obs-dstreamy-<version>-macos-universal.zip` | The OBS plugin for macOS |
| `obs-dstreamy-<version>-windows-x64.zip` | The OBS plugin for Windows, with `dstreamy.exe` |
| `D-Streamy-<version>-macos-universal.zip` | The macOS menu bar app |

On macOS, Homebrew installs the CLI and the app from the `lkshrk/tap` tap: `brew install lkshrk/tap/dstreamy` and `brew install --cask lkshrk/tap/d-streamy`. On Arch Linux, use the [pacman repository](#pacman).

### pacman

A signed pacman repository has `dstreamy` for x86_64 and aarch64 and the OBS plugin `obs-dstreamy` for x86_64. It holds only the latest release. Import the signing key, check that its fingerprint matches the one on [the repository page](https://lkshrk.github.io/d-streamy/arch/), and sign it locally:

```bash
curl -fsSLO https://lkshrk.github.io/d-streamy/arch/d-streamy.asc
gpg --show-keys d-streamy.asc
sudo pacman-key --add d-streamy.asc
sudo pacman-key --lsign-key <FINGERPRINT>
```

Add the repository to `/etc/pacman.conf`:

```ini
[d-streamy]
Server = https://lkshrk.github.io/d-streamy/arch/$arch
```

Then install the CLI, and the plugin if you use OBS:

```bash
sudo pacman -Syu dstreamy obs-dstreamy
```

The packages are named `dstreamy-bin` and `obs-dstreamy-bin` and provide `dstreamy` and `obs-dstreamy`. Screen capture needs your desktop's portal backend (`xdg-desktop-portal-impl`, for example `xdg-desktop-portal-gnome`, `-kde` or `-hyprland`). For encoding, install `gst-plugin-va` for VA-API hardware encoding, or `gst-plugins-ugly` for software x264; NVIDIA's encoder comes with `gst-plugins-bad-libs`. The plugin goes into `/usr/lib/obs-plugins/`; run `dstreamy service install` once for the service it talks to.

### Command line

Check the archive, unpack it and put `dstreamy` on your `PATH`:

```bash
shasum -a 256 -c dstreamy-<version>-<target>.tar.gz.sha256
tar -xzf dstreamy-<version>-<target>.tar.gz
install -m 755 dstreamy-<version>-<target>/dstreamy ~/.local/bin/
```

A tarball downloaded with a browser on macOS is quarantined; clear it with `xattr -d com.apple.quarantine ~/.local/bin/dstreamy`.

Requirements:

- **macOS** 14 or later, with the Screen Recording permission for the terminal that runs `dstreamy`.
- **Linux** with glibc 2.36 or later (Debian 12, Ubuntu 24.04 or newer), PipeWire, a D-Bus session and xdg-desktop-portal with a ScreenCast backend (GNOME, KDE or wlroots), plus the GStreamer plugins:

  ```bash
  sudo apt-get install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-pipewire gstreamer1.0-pulseaudio
  ```

  The release build links WebKitGTK 4.1 for browser login, so `dstreamy` won't start without `libwebkit2gtk-4.1-0 libgtk-3-0 libsoup-3.0-0` (Debian and Ubuntu package names). Hardware encoding is used when a VA-API or NVIDIA driver is present; otherwise x264 encodes in software.
- **Windows** has no screen capture; use the OBS plugin, which ships its own `dstreamy.exe`.

### OBS plugin

The plugin needs OBS Studio 32.2 or later, installed natively (Flatpak OBS can't reach the service's socket). Close OBS before installing.

- **Linux:** on Debian or Ubuntu with OBS in `/usr`, unpack the tarball's `lib/` into `/usr`. Otherwise, or for a per-user install, copy `obs-dstreamy.so` and the `dstreamy-ui/` folder next to it into `~/.config/obs-studio/plugins/obs-dstreamy/bin/64bit/`.
- **macOS:** copy `obs-dstreamy.plugin` into `~/Library/Application Support/obs-studio/plugins/`.
- **Windows:** unzip the `obs-dstreamy` folder into `C:\ProgramData\obs-studio\plugins\`.

The plugin talks to `dstreamy serve`, which holds the Discord login and session:

- **Linux:** run `dstreamy service install` once; systemd keeps the service running.
- **macOS:** run `dstreamy serve` yourself, in a terminal or from a launchd agent of your own.
- **Windows:** nothing to do. The plugin starts its bundled `dstreamy.exe serve` when needed. When OBS closes, it shuts that serve down, unless another client's Discord session is still live on it.

### macOS app

Unzip `D-Streamy-<version>-macos-universal.zip` into `/Applications`, or use the Homebrew cask. The app needs the Screen Recording permission, which macOS asks for on the first stream.

## Usage

### Command line

```bash
dstreamy login                   # scan the QR code with the Discord mobile app
dstreamy guilds                  # list your servers
dstreamy channels <SERVER>       # list a server's voice channels
dstreamy targets                 # macOS: displays and windows; Linux: portal
dstreamy audio-sources           # audio devices for --audio-source
dstreamy stream --guild <SERVER> --channel <CHANNEL> --target <TARGET>
dstreamy tui                     # the same, with a terminal UI and live health
```

Ctrl-C stops a stream and leaves Discord cleanly. `stream` takes `--res`, `--fps`, `--bitrate`, `--codec`, `--gain`, `--crop`, `--audio-source` and `--require-e2ee`; `dstreamy <command> --help` lists them all. Add `--json` for machine-readable output and `-v` or `-vv` for logs.

Other ways to log in: `dstreamy login --token` asks for a token, a token piped on stdin is used directly, and `dstreamy login --browser` opens Discord's login page in a window. Each login is saved as an account; `dstreamy account list`, `dstreamy account use <name>` and `dstreamy account remove <name>` manage them, and any command takes `--account <name>`.

Settings live in `config.toml` in the config directory: `~/.config/dstreamy` on Linux, `~/Library/Application Support/me.harke.dstreamy` on macOS, `%APPDATA%\harke\dstreamy\config` on Windows. `--config-dir` or `DSTREAMY_CONFIG_DIR` moves it. The same directory holds the saved accounts (`accounts/<name>/`, each with its token) and `identify/`, which counts Discord logins per token across every process, so that a token logs in at most 100 times a day. `tui`, `serve` and `daemon` write `dstreamy.log` in the same directory; `stream` and the other commands log to stderr (`-v`, `-vv`). All keys are in [Command line and core](docs/components/cli-and-core.md#configuration).

### OBS

1. Make sure the service runs (see [OBS plugin](#obs-plugin) above).
2. Click the D-Streamy panel in OBS's status bar, or Tools > D-Streamy…. Log in or pick an account, then choose the server and voice channel. The first time, the plugin asks before it takes over an audio track (Track 6) on your sources, so your microphone stays out of the Discord mix.
3. Press Start and confirm. While D-Streamy connects to Discord, Start becomes Cancel start, which ends the session it was bringing up. Once live, the panel shows the session's state, missed frames, audio, reconnects, viewers and duration. Stop asks first.

If OBS restarts or crashes, the Discord session keeps running for the grace period, 30 s by default (`[serve] grace_secs` in `config.toml`). Press Start again within that time to continue the same stream.

### macOS app

Click the menu bar icon, log in by pasting your Discord token, pick a server, a voice channel and a window or display, then press Start.

## Build from source

You need the Rust toolchain from `rust/rust-toolchain.toml` (rustup picks it up). On Linux also install the build packages:

```bash
sudo apt-get install clang libclang-dev pkg-config libpipewire-0.3-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
cd rust
cargo build --release -p dstreamy-cli
./target/release/dstreamy --version
```

Add `--features browser-login` for browser login; on Linux that needs `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev`. macOS needs Xcode 27.1. The OBS plugin builds through the CMake presets in `obs/`, and the macOS app with `bash scripts/build.sh` (set `CODESIGN_IDENTITY` to your own signing identity, or `SKIP_SIGN=1` for an unsigned build). [CONTRIBUTING](CONTRIBUTING.md) and [Build, test and release](docs/components/build-test-release.md) have the full setup and the test commands.

## Disclaimer

Use at your own risk. This project automates a Discord user account (a self-bot), which violates Discord's Terms of Service. Your account may be suspended or banned. The authors take no responsibility for any consequences.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). This covers the OBS plugin too, which links libobs (GPL-2.0-or-later). Vendored crates under `rust/vendor/` keep their own licences. Release archives and the macOS app include `THIRD_PARTY_NOTICES.txt` with the licence of every linked crate. Release builds contain no OpenH264 ([ADR 0010](docs/adr/0010-openh264-licensing.md)).
