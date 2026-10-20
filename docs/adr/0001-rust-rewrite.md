# 0001 — Rewrite D-Streamy as a Rust CLI/TUI with per-OS capture backends

Status: accepted (2026-10-20)

## Context

D-Streamy is a SwiftUI menu bar app (ScreenCaptureKit + VideoToolbox) feeding a Bun daemon that speaks Discord Go Live through `@dank074/discord-video-stream`. Only the daemon is portable; about 3.7k lines of Swift are tied to macOS. The goal is one codebase that runs on macOS and Linux.

## Decision

- Rewrite everything in Rust, as a Cargo workspace under `rust/`, built on the `rust-rewrite` branch.
- The UI is a CLI with a TUI (`ratatui`); the menu bar app is dropped.
- Capture and encode sit behind a `CaptureSource` trait, with compile-time backends selected by `cfg(target_os)`; no runtime dylib plugins (Rust has no stable ABI).
  - macOS: `screencapturekit` + VideoToolbox.
  - Linux: `ashpd` (ScreenCast portal) + PipeWire + GStreamer (VA-API/NVENC/x264).
- Discord transport: vendor `discord-stream-rs` (a Rust port of `discord-video-stream`, with DAVE via `davey` and WebRTC via `webrtc-rs`), with the FFmpeg feature off and pre-encoded Annex B frames fed in. We write our own user-token main gateway client on `tokio-tungstenite`, since the library leaves the gateway to the caller.
- Verification runs against a local mock Discord server (gateway, voice WS, WebRTC peer, DAVE peer). The real Discord, real capture permission and GPU encoders are checked once by hand, from `.scratch/rust-rewrite/MANUAL-CHECKLIST.md`.

## Consequences

- A single static binary, no Bun runtime, no native-module bundling traps.
- `discord-stream-rs` is young (v0.1.3, one maintainer), so it is vendored and we own its fixes; `@dank074/discord-video-stream` stays the reference for protocol changes.
- A passing mock does not prove Discord compatibility; protocol drift shows up only in the manual live check.
- The crop overlay becomes a numeric crop rectangle. On Wayland, the window is picked in the portal dialog (its restore token is persisted).
- Swift and Bun code stay in the tree as reference until the manual checklist passes, then they are removed.

## Rejected

- Porting Swift to Linux: no AppKit, SwiftUI or ScreenCaptureKit there.
- Keeping the Bun daemon as a sidecar: a viable Rust transport exists.
- Songbird or cacophony: no Go Live or video.
