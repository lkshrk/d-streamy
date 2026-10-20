# 04 — vendor discord stream rs

Status: done
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

Evidence (2026-10-21, branch `worktree-agent-a4d49c7726c329a0a`, on top of `rust-rewrite` `2ff2ce6`):

- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` on macOS: pass. Vendor tests: 23 unit, `tests/streamer_flow.rs` 3, `tests/webrtc_loopback.rs` 1.
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-04 rust/ci/linux.sh`: exit 0, same test counts. The image has no FFmpeg (`pkg-config --exists libavcodec` exits 1).
- macOS: `cargo tree --workspace -e normal | grep -ci ffmpeg` = 0; `otool -L` of the test binary links only Security, CoreFoundation, libiconv, libSystem.
- Audit: `.scratch/rust-rewrite/notes/protocol-audit.md`; each difference is patched (commit listed) or justified.
- Vendor commits: `4e0f597` import, `46e4974` FFmpeg opt-in + workspace, `400c50b` fmt/clippy, `b36d26a` binary framing, `7801232` DAVE version in IDENTIFY, `315dd8e` DAVE transitions, `2ae9eeb` SPS fail-safe, `b95205b` webrtc import, `299e81a` webrtc SSRC hook, `684e30b` WebRTC transport parity, `a01cea0` connection lifecycle + Streamer, `1820e60` opcodes, `cdee276` H264/H265 only, `4c462c1` cancel-safe ready, `fa94b91` scaling lists, `a33107b` reconnect/heartbeat/queue hardening.
- Self-review: reviewer subagent on the vendor diff; confirmed findings (scaling-list parse, Exp-Golomb overflow, ready hang on answer failure, reconnect hot loop, heartbeat ACK, unbounded pacer queue) fixed in `fa94b91` and `a33107b`.
- Send API (`discord_stream_rs::MediaConnection`, re-exported as `dstreamy_discord::transport`): `send_video_frame(&self, frame: &[u8], frametime_ms: f64)`, `send_audio_frame(..)`, `send_video_frame_at(&self, frame: &[u8], pts: Duration)`, `send_audio_frame_at(..)`, all `-> Result<(), WebRtcError>`.

Decision: vendored from the crates.io 0.1.3 tarball (upstream commit `50a402c`, MIT); provenance and checksum in `rust/vendor/README.md`.
Decision: `discord-stream-rs` is a workspace member with `default = []` (FFmpeg behind the opt-in `media` feature), so it gets the same fmt/clippy/test gates; it was formatted and clippy-fixed rather than excluded.
Decision: webrtc 0.14.0 vendored under `rust/vendor/webrtc` via `[patch.crates-io]` (excluded from the workspace) for one hook, `TrackLocal::preferred_ssrc()`, so RTP/NACK/SR use Discord's READY SSRCs. Recorded as ADR 0002.
Decision: upstream 0.1.3 was a partial port (stubbed signalling, no SELECT_PROTOCOL, no heartbeat, one RTP packet per frame); the voice/stream connection, Streamer and WebRTC sender were rewritten to the JS reference instead of patched line by line. Large fixes are therefore single commits per area, not per field.
Decision: own RTP packetizer (single NAL or FU-A/FU, 1200-byte payloads, never drop or aggregate NAL units) instead of webrtc-rs's `H264Payloader`, which drops AUD/filler and builds STAP-A and would break DAVE's per-NAL unencrypted ranges.
Decision: the synthesised answer uses `a=recvonly` (not the reference's `a=inactive`) and adds session lines, because webrtc-rs honours direction and needs them to parse.
Decision: only H264 and H265 packetizers; AV1/VP8/VP9 return `UnsupportedCodec` (dstreamy streams H264/H265).
Decision: `send_*_frame_at(pts)` added beside the JS-style `frametime` API; pts maps to `start_timestamp + pts * clock_rate`.
Decision: deliberate hardening beyond the reference: bounded reconnects (1 s delay, 5 attempts), heartbeat-ACK timeout (3 missed), bounded video pacer queue, malformed SPS passed through instead of throwing.
Decision: ICE servers and the webrtc-rs `SettingEngine` are configurable on `Streamer`, and `ws://`/`wss://` endpoints are used verbatim, so the mock server (issue 05) can run on loopback without STUN.
Decision: `dstreamy-discord` only re-exports the vendored crate as `transport`; the gateway client stays issue 06.
