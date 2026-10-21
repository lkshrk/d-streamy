# 08 — session supervisor

Status: done
Depends: 07

## Scope

`Session` state machine (lives in `dstreamy-discord::session`, see Decision below) (Idle → Connecting → Streaming → Reconnecting → Stopped) driving streamer: auto-reconnect with backoff, channel move follow (patch channelId, no rejoin — vault pitfall `discord-channel-move`), graceful shutdown (vault `daemon-epipe-log-loop`: logging must never loop on broken output), health metrics events stream. File logger with size cap.

## Gates

- Mock fault tests: voice WS drop → resumes streaming; server channel move → continues without rejoin; stop → clean teardown, no leaked tasks (tokio test with timeout)
- Global gates from PRD

## Comments
- Carried from 07 (coordinator, 2026-10-21):
  - Health is poll-only (`StreamHandle::stats`). Add a status watch / `closed()` future with typed reason.
  - Split frame pump from Discord session so reconnect/channel move doesn't restart the source.
  - Return the source from `stop()` and on errors so retries can reuse it.
  - Route viewer PLI to `request_keyframe` (vendored transport ignores PLI today).
  - Timebase offset shared across tracks; a single-track restart shifts the other.
  - Streamer holds one Go Live session; supervisor must own exactly one live handle.
  - Vault pitfall `davey-first-frame-nonce`: first video keyframe can be lost at start; request a keyframe shortly after DAVE becomes active.
- Evidence (2026-10-21, commits `f5b44f0`, `e15fd04`, `c79f231` on `rust-rewrite`):
  - `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` on macOS: pass, 221 passed / 5 ignored.
  - `CARGO_BUILD_JOBS=2 rust/ci/linux.sh` (volume `dstreamy-linux-target`): pass, 216 passed / 1 ignored.
  - `cargo test -p dstreamy-discord --test session --test stream` 10 runs in a row: 18 + 6 passed every run, no flakes.
  - `dstreamy-discord/tests/session.rs` (18, all against the mock): voice WS drop resumes in place (no state change, one STREAM_CREATE); lost stream connection (4006) recreates Go Live with the same source (one source start); lost voice connection rejoins (two op 4 joins) with the same source; channel move sends STREAM_DELETE for the old key and STREAM_CREATE for the new channel with no new op 4; gateway drop resumes with the stream untouched; invalid session re-identifies, rejoins and streams; health snapshots every second with fps 20–40, audio ≥ 30 frames/s, scores 0–100; first keyframe request ≥ 400 ms after streaming (DAVE) and a viewer PLI from the mock reaches `request_keyframe` and `viewer_keyframe_requests`; `update_video` op 12 survives reconnects, also when called while reconnecting; give up after the retry limit; stop while reconnecting, second start refused with its source, source start failure, ended capture and a rejected gateway (4004) all end in `Stopped` with the source handed back; `stop` returns the session's tasks to the runtime baseline; state and events serialize to JSON.
  - `dstreamy-core::logging` (7): append and reopen at size, rotation caps every file and keeps `keep` old files, keep 0 truncates, unwritable path swallowed, broken output written once then dropped, `WouldBlock` does not latch, tracing writes through the rotating file and a broken stderr is tried once.
  - Vendor: loopback test asserts a PLI from the remote peer bumps `keyframe_requests()`.
  - Self-review: reviewer subagent on the diff. Fixed: a close during stream setup is now an error; attributes changed while reconnecting are announced on the new connection; a failed stream-only retry escalates to rejoining voice; viewer PLIs throttled to one keyframe per 500 ms; health only sampled while streaming, sampler reset on every attach; no keyframe requests while detached; a cancelled `stop()` keeps the source; `QuietOnError` ignores `WouldBlock`; `StartError` type and `Serialize`/`Display` on state, events and health; multi-line and restating comments; weak DAVE and broken-stderr assertions; missing tests listed above.
- Public API (`dstreamy_discord`): `Session::{new(Arc<DiscordClient>), state() -> watch::Receiver<SessionState>, subscribe() -> broadcast::Receiver<SessionEvent>, start(guild, channel, Box<dyn CaptureSource>, SessionOptions) -> Result<(), StartError>, stop() -> Result<Option<Box<dyn CaptureSource>>, SessionError>, update_video(w, h, fps), request_keyframe(), shutdown(self) -> Option<Box<dyn CaptureSource>>}`; `SessionState {Idle, Connecting, Streaming, Reconnecting{attempt}, Stopped{reason: StopReason}}`; `StopReason {Requested, CaptureEnded, GatewayClosed, GaveUp{attempts}, SourceFailed{message}}`; `SessionEvent {State, Health(HealthSnapshot{metrics, health, stats}), ChannelMoved, Resumed, Error{message, fatal}}`; `SessionOptions {stream, backoff: Backoff{base, max, max_attempts}, health_interval, health_history}`; `StreamHandle::stop_and_take_source()`; `StreamStats::viewer_keyframe_requests`. `dstreamy_core::{RotatingFileWriter, QuietOnError}`; `move || writer.clone()` is a tracing `MakeWriter`.
- Decision: `Session` lives in `dstreamy-discord` (module `session`), not `dstreamy-core`: it drives `start_stream`'s pieces and the `Streamer`, and `dstreamy-discord` now depends on `dstreamy-core` for health scoring, so the other direction would be a cycle.
- Decision: the media pump (source, reader, queue, sender, keyframe watcher) is split from the Discord connection; a reconnect or channel move detaches it (frames counted as dropped), and `attach` drops queued video and requests a keyframe. The capture source is started once per session.
- Decision: reconnect follows `src/core/stream.ts`: jittered backoff 1 s doubling to 30 s, 10 attempts, also for the first connect. A lost stream connection recreates Go Live only; a lost voice connection, a new gateway session (READY after an invalid session) or a failed retry rejoins voice. A closed gateway is fatal (`Stopped{GatewayClosed}`); the CLI creates a new client.
- Decision: channel move: STREAM_DELETE (old key) → `set_voice_channel` → `create_stream`, no op 4; order matters because the Streamer derives the stream key from the current channel. A null-channel VOICE_STATE_UPDATE while streaming rejoins; while reconnecting it is ignored (reference).
- Decision: keyframe 500 ms after DAVE becomes ready on the attached connection (davey first-frame pitfall); viewer PLI/FIR at most one keyframe per 500 ms.
- Decision: health is sampled every `health_interval` (1 s; `HealthSample::audio_score` assumes 1 s intervals) while streaming only, history capped at 60 like the app.
- Decision: the file logger rotates to `.1` instead of truncating like `file-logger.ts`: 10 MB per file, one old file kept, write errors swallowed. `QuietOnError` latches on the first failed write (EPIPE) except `WouldBlock`.
- Decision: per-track timebase offsets; a track that restarts adopts the other track's latest rebase so a restart of both stays aligned.
- Follow-ups for issue 09 (CLI): use `QuietOnError` for stdout/stderr, never `println!`/`eprintln!` (they panic on EPIPE), and exit when the output pipe breaks; keep `health_interval` at 1 s; `--json` can serialize `SessionEvent`/`SessionState` directly; `start_stream` still drops its source on failure (Session does not); move/removal detection matches the user id, not the session id; a lagged gateway broadcast is only logged (a missed move would go unnoticed); the vendor counts PLI on both senders' RTCP readers (one count observed in the loopback test) and FIR is untested.
