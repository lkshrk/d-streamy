# Overview

D-Streamy streams a window, a display or OBS's output to a Discord voice channel through Go Live, Discord's screenshare. It logs in as a Discord user account, joins the voice channel, creates the Go Live stream and sends H.264 video and Opus audio to it.

Everything runs through one binary, `dstreamy`. Three frontends sit on top of it:

- **The command line and TUI.** `dstreamy stream` captures a window or display and streams it until Ctrl-C. `dstreamy tui` does the same with a terminal interface for picking the channel and target and watching stream health.
- **The OBS plugin.** A panel in OBS's status bar starts a Discord stream of OBS's own encoded output, next to a normal Twitch or YouTube stream. The plugin holds no Discord code and no token: it sends its packets to `dstreamy serve`, a long-running local service.
- **The macOS menu bar app.** A SwiftUI app that runs the bundled `dstreamy daemon` as a child process and drives it over JSON on stdin and stdout.

D-Streamy automates a user account, which breaks Discord's Terms of Service. See the [disclaimer](../README.md#disclaimer).

## Platforms

| Platform | Native capture | OBS plugin | Service supervision | macOS app |
|---|---|---|---|---|
| Linux (x86_64, aarch64) | Portal picker, PipeWire, GStreamer | Yes (x86_64 package) | `dstreamy service` (systemd user unit) | – |
| macOS 14+ (arm64, x86_64) | ScreenCaptureKit, VideoToolbox | Yes | Run `dstreamy serve` yourself | Yes |
| Windows (x86_64) | None | Yes | The plugin starts its bundled `dstreamy.exe` | – |

Windows streams only from the OBS plugin. Details per platform are in [Media and capture](components/media-and-capture.md) and [Service and IPC](components/service-and-ipc.md).

## Features

- Window or display capture with an optional crop. On Linux the window or screen is picked in the desktop portal dialog.
- OBS's output as the source, from a status-bar panel, without touching OBS's normal stream.
- Resolution up to 1080p, 15, 30 or 60 fps, configurable bitrate, audio gain and audio device.
- H.264 by default. H.265 is available for native capture but is experimental: most Discord viewers cannot decode it.
- Several saved accounts with a default account; two accounts can stream at once from two processes.
- Stream health: video and audio scores, drops, reconnects and viewers in the TUI, the OBS panel and the macOS app.
- Automatic reconnects, and the stream follows when the account is moved to another voice channel.
- DAVE end-to-end encryption, reported as unknown, encrypted or unencrypted; `require_e2ee` refuses to stream without it.

## Technology

- **Rust** (toolchain pinned in `rust/rust-toolchain.toml`) for everything except the two UIs: tokio for async, clap for the command line, ratatui for the TUI.
- **Discord transport:** vendored `discord-stream-rs` and a patched `webrtc-rs` in `rust/vendor/` ([ADR 0002](adr/0002-vendor-webrtc-for-discord-ssrcs.md)).
- **Capture:** GStreamer and PipeWire through the xdg-desktop-portal on Linux; ScreenCaptureKit and VideoToolbox on macOS.
- **Audio encoding:** `unsafe-libopus`, a Rust translation of libopus, so no system libopus is needed.
- **OBS plugin:** a Rust `cdylib` against libobs (bindgen), built by the obs-plugintemplate CMake project, with a separate C++/Qt library for the panel.
- **macOS app:** SwiftUI, built with Swift Package Manager and bundled by `scripts/build.sh`.

## Repository layout

```text
rust/                       Cargo workspace that builds dstreamy
  crates/dstreamy-cli       the dstreamy binary: CLI, TUI, daemon, serve, service
  crates/dstreamy-core      configuration, accounts on disk, logging, health levels
  crates/dstreamy-media     the CaptureSource interface, media frames, test pattern, external source
  crates/dstreamy-capture-linux   portal, PipeWire and GStreamer capture
  crates/dstreamy-capture-macos   ScreenCaptureKit and VideoToolbox capture
  crates/dstreamy-discord   gateway, voice, Go Live, DAVE, the media pump
  crates/dstreamy-ipc       the local socket and named-pipe transport and wire codecs
  crates/dstreamy-obs-producer    the OBS plugin's libobs-free half
  crates/dstreamy-obs-packet      turns OBS encoded packets into producer frames
  crates/dstreamy-mock-discord    a fake Discord for tests
  crates/dstreamy-test-producer   a fake producer and trace replay for tests
  crates/dstreamy-obs       the OBS plugin module (its own workspace)
  crates/dstreamy-obs-sys   libobs bindings (part of the plugin workspace)
  vendor/                   discord-stream-rs and webrtc-rs
obs/                        CMake project for the plugin, the Qt panel, the harness, traces
App/, AppTests/             the macOS menu bar app and its tests
scripts/                    app build, release helpers, licence notices
Formula/, Casks/            Homebrew templates filled at release
docs/                       this documentation, ADRs and specs
```

## Where to go next

- To use D-Streamy: the [README](../README.md).
- To understand how it fits together: [Architecture](architecture.md) and [Workflows](workflows.md).
- To work on it: [Build, test and release](components/build-test-release.md) and [CONTRIBUTING](../CONTRIBUTING.md).
