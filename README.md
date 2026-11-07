# D-Streamy

Streams a window, a display or OBS's output to a Discord voice channel through Go Live (screenshare), on Linux and macOS.

Everything runs through one binary, `dstreamy`:

- **`dstreamy`** (`rust/`): the CLI and TUI, the capture and encode backends (PipeWire and GStreamer on Linux, ScreenCaptureKit and VideoToolbox on macOS), the Discord protocol, and `dstreamy serve`, the service that OBS talks to. Released together with the app and the plugin from one `vX.Y.Z` tag ([Releasing](CONTRIBUTING.md#releasing)).
- **OBS plugin** (`obs/`, `rust/crates/dstreamy-obs`): a status-bar panel in OBS that sends OBS's encoded H.264 and Opus to a running `dstreamy serve` ([ADR 0004](docs/adr/0004-persistent-service-and-external-producers.md), [ADR 0007](docs/adr/0007-obs-status-bar-panel.md)).
- **macOS menu bar app** (`App/`): a thin SwiftUI frontend that spawns the bundled `dstreamy daemon` and talks to it over JSON on stdin and stdout ([ADR 0003](docs/adr/0003-macos-menu-bar-on-rust-daemon.md)).

Design decisions are in [`docs/adr/`](docs/adr/); every external behaviour the service and the plugin rely on is pinned in [`docs/specs.md`](docs/specs.md).

## Features

- **Window or display capture**, with a numeric crop; on Linux the window or screen is picked in the desktop portal dialog.
- **OBS as the source**: stream OBS's canvas to Discord next to a Twitch stream, from a panel in OBS's status bar.
- **Configurable** resolution (up to 1080p), FPS (15, 30 or 60), bitrate, audio gain and audio device.
- **Codec** H.264 (default) or H.265. **Note: Discord does not currently support H.265 for viewing in practice.** Its SFU forwards without transcoding, and most desktop and web clients cannot decode incoming HEVC, so H.265 viewers typically see no video. Leave this on H.264 unless you know every viewer can decode HEVC.
- **Several accounts**: saved logins by name, a default account, and two accounts streaming at once from two processes.
- **Stream health**: video and audio scores, drops, reconnects and viewer count in the TUI, the OBS panel and the macOS app.
- **Auto-reconnect** on disconnect; the stream follows when the account is moved to another voice channel.

## Install

Download `dstreamy-<version>-<target>.tar.gz` for your platform from the GitHub release, check it against the `.sha256` file next to it (`sha256sum -c` works too), and put `dstreamy` on your `PATH`:

```bash
shasum -a 256 -c dstreamy-0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf dstreamy-0.1.0-aarch64-apple-darwin.tar.gz
install -m 755 dstreamy-0.1.0-aarch64-apple-darwin/dstreamy ~/.local/bin/
```

Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. A tarball downloaded with a browser on macOS is quarantined; clear it with `xattr -d com.apple.quarantine ~/.local/bin/dstreamy`.

Requirements:

- A Discord user token (see [Disclaimer](#disclaimer)).
- macOS 14+ with Screen Recording permission for the terminal that runs `dstreamy`.
- Linux with glibc 2.36+ (Debian 12, Ubuntu 24.04 or newer), PipeWire, a D-Bus session and xdg-desktop-portal with a ScreenCast backend (GNOME, KDE or wlroots), plus GStreamer:

  ```bash
  sudo apt-get install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-pipewire gstreamer1.0-pulseaudio
  ```

  Desktop release builds include browser login (`dstreamy login --browser`). On Linux it needs the WebKitGTK 4.1 runtime (Debian/Ubuntu: `sudo apt-get install libwebkit2gtk-4.1-0 libgtk-3-0 libsoup-3.0-0`); without it the binary fails to start. QR login (`dstreamy login`) remains the normal path, and a build without the feature needs none of these.

  Hardware encoding is used when available: VA-API needs a VA driver for your GPU (and `gstreamer1.0-vaapi` for the older elements), NVENC needs the NVIDIA driver. Otherwise `x264enc` encodes in software ([ADR 0006](docs/adr/0006-gstreamer-element-selection.md)).

## Usage

```bash
dstreamy login                      # scan the QR code with the Discord mobile app; the token is checked, then saved as an account
dstreamy guilds                     # list the account's servers
dstreamy channels <SERVER>          # list the voice channels of a server
dstreamy targets                    # list capturable displays and windows
dstreamy audio-sources              # list the audio devices a stream can capture
dstreamy stream --guild <SERVER> --channel <CHANNEL> --target display:1
dstreamy tui                        # pick a channel and target, adjust settings and watch stream health
```

**Login and accounts.** `dstreamy login --token` prompts for a token instead, and a token piped on stdin (`dstreamy login < token.txt`) is used without a prompt. Builds with the `browser-login` feature also offer `dstreamy login --browser`, which logs in on Discord's login page in a window ([ADR 0008](docs/adr/0008-browser-login-webview.md)); other builds say so and point to the QR or token login. Each login is saved as an account, named after the Discord username unless you pass `--account <name>`; logging in again as the same user updates that account. `dstreamy account list`, `dstreamy account use <name>` (the default for commands without `--account`) and `dstreamy account remove <name>` manage them, and any command takes `--account <name>`. Two accounts can stream at once, for example OBS through `dstreamy serve` with one and `dstreamy stream --account <other>` with the other; the same account twice is refused. A token saved by an older version moves into an account on first use. `DSTREAMY_TOKEN` overrides the saved accounts ([ADR 0005](docs/adr/0005-multiple-accounts.md)).

**Streaming.** `stream` runs until Ctrl-C and takes `--codec h264|h265` (h265 is experimental: most Discord viewers can't decode it), `--fps 15|30|60`, `--res 720p|900p|1080p|WIDTHxHEIGHT`, `--bitrate <kbps>`, `--gain <0.0-3.0>`, `--crop X,Y,W,H` and `--audio-source <device>` (a name from `dstreamy audio-sources`; without it the stream captures what plays on the default output). `--target test-pattern` streams a diagnostic pattern without capturing anything. It needs a build with `--features test-pattern`; release builds ship no H.264 software encoder ([ADR 0010](docs/adr/0010-openh264-licensing.md)). On Linux, use `--target portal` and pick the window or screen in the portal dialog. Add `--json` for machine-readable output and `-v`/`-vv` for logs. See `dstreamy <command> --help` for details.

**Encryption.** Add `--require-e2ee` to `stream`, or set `require_e2ee = true` under `[stream]`, to stop gracefully if Discord cannot provide DAVE end-to-end encryption. A request cannot weaken the configured requirement. Without that requirement, an unencrypted session may proceed with hop-by-hop SRTP only, which lets Discord read the media. The macOS app and OBS panel distinguish unknown, encrypted and unencrypted reported states: unknown is not proof of encryption, and a nonzero DAVE protocol version alone is not proof either. OBS warns visibly when Discord can read the stream and includes the reported version in its health details. The TUI reports encryption changes; older producers may leave the information unknown.

**Configuration.** Settings live in `config.toml` in the config directory, which `--config-dir <DIR>` or `DSTREAMY_CONFIG_DIR` overrides. `[stream]` holds the defaults for the flags above, `target`, `crop` and `audio_source` the last picks, and `[serve]` tunes the service:

```toml
[stream]
require_e2ee = false

[serve]
grace_secs = 30     # how long a session survives without media from OBS (at most 3600)
idle = "documented" # what Discord gets after a producer detached: "documented", "none" or "continuous"
stall_ms = 2000     # after this long without a frame an attached producer counts as stalled (500 to 600000)
```

## Service and OBS

### `dstreamy serve`

`dstreamy serve` is a long-running service. The OBS plugin connects to it over a local socket in a private per-user directory (`$XDG_RUNTIME_DIR/dstreamy/` on Linux, the per-user temporary directory returned by `confstr(_CS_DARWIN_USER_TEMP_DIR)` plus `dstreamy/` on macOS); `--endpoint <PATH>` moves the socket, but its parent must be an owned 0700 directory under trusted parents, never `/tmp`. The default falls back to `~/.local/state/dstreamy/` on Linux or `~/Library/Application Support/dstreamy/` on macOS when needed. macOS does not derive the path from environment `TMPDIR`. It runs until a client sends `shutdown` or it gets SIGINT or SIGTERM, keeps the Discord stream alive across an OBS restart for `grace_secs`, and logs to `dstreamy.log` in the config directory. It must be running before you press Start in OBS.

On Linux, `dstreamy service` keeps it running under systemd:

```bash
dstreamy service install     # write the user unit, enable and start it (restarted only when the unit changed)
dstreamy service status      # installed? running? does the socket answer?
dstreamy service print       # show the unit install would write
dstreamy service uninstall   # stop, disable and remove it
```

`service` is Linux only today; on macOS run `dstreamy serve` yourself, from a terminal or a launchd agent of your own.

### OBS plugin

The plugin can be built from source. `release.yml` builds it for Linux, macOS and Windows for every `vX.Y.Z` release and attaches the Linux and macOS packages; the Windows package is built but not published. No published tag, completed rehearsal or signing run has been verified here. Check the actual release assets before relying on a download. OBS 32.2.2 is the build baseline; `obs-ci.yml` also defines Linux plugin tests against OBS 33.0.1. macOS and Windows jobs use the baseline, and Windows still cannot connect to the service. Flatpak OBS cannot reach the service's socket in its default sandbox, so use a native OBS package. Windows is not supported yet: the panel says so and disables Start and Stop.

Build it against an OBS build or the OBS development packages (`cd obs`, then `cmake --preset ubuntu-x86_64` or `cmake --preset macos`, then `cmake --build --preset <the same>`; CI's exact steps are in `.github/workflows/obs-ci.yml`), then install the two modules where OBS looks for user plugins:

- Linux: `cmake --install build_x86_64 --prefix <prefix>` puts `obs-plugins/obs-dstreamy.so` and `obs-plugins/obs-dstreamy-ui/obs-dstreamy-ui.so` under `<prefix>/lib/<triplet>/`. For a per-user install, copy them to `~/.config/obs-studio/plugins/obs-dstreamy/bin/64bit/`, keeping `obs-dstreamy-ui/` as a subfolder next to `obs-dstreamy.so`.
- macOS: the build produces `obs-dstreamy.plugin` (the UI library sits inside its `Contents/MacOS/`); copy the bundle to `~/Library/Application Support/obs-studio/plugins/`.

The release workflow packages Linux as a prefix-relative tarball with `lib/x86_64-linux-gnu/obs-plugins/`, macOS as an `obs-dstreamy.plugin` zip, and Windows as an `obs-dstreamy` folder zip for `%ProgramData%\obs-studio\plugins\`. Keep the helper and notices with the module. A Windows package does not enable Windows streaming. See [obs/README.md](obs/README.md) for Qt compatibility and trace guidance.

In OBS:

1. Make sure `dstreamy serve` is running (`dstreamy service status` on Linux).
2. Open Tools > D-Streamy… to log in or pick an account, choose the server and voice channel, and set the video encoder mode and the Discord audio track. The first time, the plugin asks before it takes over Track 6 on your audio sources, so the mix sent to Discord has no microphone.
3. Press Start in the status-bar panel. The panel shows the session state, missed frames, audio state, reconnects, viewers and the stream's duration; Stop asks first. Audio details list packets dropped in OBS separately from video losses and audio-dropout intervals. If the output fails but its Discord session could not be ended, the warning and Stop control remain until a successful Stop.

Your normal stream to Twitch or elsewhere is untouched: the plugin registers its own output with its own Opus encoder and, by default, its own H.264 encoder. Sharing the stream's video encoder is an option that works while that stream runs and its encoder meets Discord's limits (H.264, no B-frames, keyframes every 1 to 2 s, up to 1920×1080 at 15, 30 or 60 fps).

## macOS app

The menu bar app needs macOS 14+ and the Screen Recording permission (asked on the first stream). Build it with Xcode 27.1 and a Rust toolchain:

```bash
bash scripts/build.sh
```

Output: `bin/D-Streamy.app`, with `dstreamy` in `Contents/MacOS/`. `DSTREAMY_DAEMON=<path>` bundles a prebuilt `dstreamy` instead of building one. When the app runs outside a bundle (`swift run`), it uses `DSTREAMY_BIN` or else `rust/target/debug/dstreamy`.

1. Launch the app (it appears in the menu bar).
2. Click Login and paste your Discord token (a token from an older version is moved over automatically).
3. Select the server and voice channel.
4. Pick a window or display to stream.
5. Click Start.

## Build from source

macOS needs Xcode 27.1 (CI and releases build with it; `screencapturekit` needs at least the Xcode 26 SDK) and a Rust toolchain (`rustup` reads `rust/rust-toolchain.toml`):

```bash
cd rust
cargo build --release -p dstreamy-cli
./target/release/dstreamy --version
```

Linux additionally needs the GStreamer and PipeWire development packages:

```bash
sudo apt-get install clang libclang-dev pkg-config libpipewire-0.3-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
cd rust
cargo build --release -p dstreamy-cli
```

Opus is encoded by `unsafe-libopus`, a Rust translation of libopus, so no libopus package is needed.

Or build the Linux binary in Docker from any host (the output lands in the `dstreamy-linux-target` volume, not in `rust/target`):

```bash
rust/ci/linux.sh cargo build --release -p dstreamy-cli
```

Browser login (`dstreamy login --browser`) is included in the desktop release tarballs (Linux and macOS) but is an opt-in build feature, so a source build without `--features browser-login` needs no webview. On Linux it needs WebKitGTK 4.1 (`libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev` to build) and links against it:

```bash
cargo build --release -p dstreamy-cli --features browser-login
```

The OBS plugin lives in its own Cargo workspace (`rust/crates/dstreamy-obs`) and is built only through the CMake presets in `obs/` (above). See [CONTRIBUTING.md](CONTRIBUTING.md) for the test commands.

## Disclaimer

Use at your own risk. This project uses a self-bot (user token automation) which violates Discord's Terms of Service. Your account may be suspended or banned. The authors take no responsibility for any consequences.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). This covers the OBS plugin in `obs/` too, which links libobs (GPL-2.0-or-later). Vendored crates under `rust/vendor/` keep their own licenses. Release archives and the macOS app include `THIRD_PARTY_NOTICES.txt`, generated by `scripts/third-party-notices.py`, with the license of every crate linked into `dstreamy`, including libopus (BSD-3-Clause, as `unsafe-libopus`). Release builds include no OpenH264; see [ADR 0010](docs/adr/0010-openh264-licensing.md).
