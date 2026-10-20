# 04 — vendor discord stream rs

Status: ready-for-agent
Depends: 01

## Scope

Vendor `discord-stream-rs` 0.1.3 (github Tky567/stream-discord-rs) into `rust/vendor/discord-stream-rs`, record upstream commit + license in `rust/vendor/README.md`. Build with `ffmpeg-next` feature off.
Audit against `node_modules/@dank074/discord-video-stream` v6.0.0: write `.scratch/rust-rewrite/notes/protocol-audit.md` listing opcodes, voice gateway version, DAVE flow, encryption modes, RTP/H264/H265 packetization differences. Patch vendor for gaps found (each patch = separate commit).
Ensure an API exists to send pre-encoded Annex B + Opus frames with caller pts.

## Gates

- Vendor crate compiles in workspace on macOS + Linux without FFmpeg libs installed
- protocol-audit.md exists, every listed difference either patched or justified
- Unit tests of vendor crate pass
- Global gates from PRD

## Comments
