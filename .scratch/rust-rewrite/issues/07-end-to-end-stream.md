# 07 — end to end stream

Status: done
Depends: 03,05,06

## Scope

Wire gateway + vendored streamer + `CaptureSource`: join voice, create Go Live, pump frames with correct pacing/pts, codec H264 and H265, update video attributes on resolution change (parity with `updateVideo` IPC).

## Gates

- E2E test: TestPatternSource → mock; mock decodes ≥ 95% of sent video AUs bit-identical and audio frames, for H264 and H265
- DAVE path exercised (mock verifies E2EE decrypt)
- Global gates from PRD

## Comments
Evidence (2026-10-21, commits `220096a`, `4365a7c` on `rust-rewrite`):

- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` on macOS: pass, 193 passed / 5 ignored (19 new: 8 queue + 5 timebase unit tests, 6 in `dstreamy-discord/tests/stream.rs`). Final state after the review round: commits up to the `fix(discord)` one below. The stream suite ran 20+ times in a row without a failure.
- `rust/ci/linux.sh` (default volume, run with `-j 2` because the first run's linker was OOM-killed in the Docker VM): pass, 188 passed.
- `h264_test_pattern_streams_through_the_mock`: `TestPatternSource` (paced) → `start_stream` → real `DiscordClient` over zlib-stream → mock. DAVE active on the stream connection; ≥ 95 % of sent AUs reassemble bit-identically (after the SPS VUI rewrite), every frame E2EE, video RTP timestamps strictly increasing, ≥ 95 % of Opus frames arrive in order; op 5 speaking=2; op 12 with the initial and, after `update_video(w/2, h/2, 15)`, the new attributes; STREAM_DELETE on stop.
- `h265_replay_streams_through_the_mock`: same gates for H265 with `tests/fixtures/h265-320x240.aus` (90 VideoToolbox AUs, keyframes at 0 and 45, 197 KB) replayed in real time by a test `ReplaySource`, plus a 440 Hz Opus tone.
- `stop_ends_every_task_it_started`: mock on its own runtime; `stop()` within a timeout, then the client runtime's `num_alive_tasks()` returns to the pre-start baseline.
- `transport_lag_drops_video_until_a_requested_keyframe`: an unpaced source of 100 KB AUs overruns the pump; video is dropped, the source gets `request_keyframe`, the next keyframe goes out and sending continues.
- `dropping_the_handle_ends_go_live_and_stops_the_source`, `cancelled_start_leaves_voice`: STREAM_DELETE / leave on Drop, source stopped; a `start_stream` cancelled after 1 ms leaves voice and creates no stream.
- Self-review: reviewer subagent on the diff; fixed: `start_stream` and `stop()` cancel-safety (teardown guard, task handles kept until joined), source stop bounded by a timeout after Discord teardown, an ended event channel now marks `connection_closed`, the 10 s DAVE wait skipped when SELECT_PROTOCOL_ACK reports DAVE version 0, jitter bumps of one RTP tick instead of 1 µs, an in-order matcher that tolerates gaps, an audio check that is no longer trivially true, tighter fps assertion, doc comments that restated types; teardown scoped to the handle's own connections (`MediaConnection::ptr_eq`, added to the vendor) so a stale handle cannot end a newer session; Drop also stops the source.

Public API (`dstreamy_discord`, module `stream`):
- `start_stream(client: Arc<DiscordClient>, guild_id: Option<String>, channel_id: String, source: Box<dyn CaptureSource>, options: StreamOptions) -> Result<StreamHandle, StreamError>`
- `StreamOptions { capture: CaptureConfig, step_timeout, dave_timeout, max_queued_video, max_queued_audio }` (`StreamOptions::new(capture)`; 30 s, 10 s, 8, 25)
- `StreamHandle::{stats() -> StreamStats, update_video(w, h, fps), video_attributes(), request_keyframe(), stop() -> Result<StreamStats, StreamError>}`; `Drop` aborts the pump and ends Go Live.
- `StreamStats { video_frames_sent, video_frames_dropped, audio_frames_sent, audio_frames_dropped, bytes_sent, keyframe_requests, resumes, fps, last_error, capture_ended, connection_closed }`
- `StreamError::{Timeout, Streamer, Connection, Media}`

Decision: the streaming layer is a `stream` module in `dstreamy-discord` (now depending on `dstreamy-media`), not a new crate: it is the adapter between the vendored transport and `CaptureSource`, which is what the PRD gives this crate; the `Session` state machine stays in `dstreamy-core` (issue 08).
Decision: join order and configuration follow `src/core/stream.ts`: join voice, create stream, then `set_packetizer(codec)`, `set_speaking(true)`, `set_video_attributes`. Before configuring, wait for the peer connection (step timeout) and give DAVE up to `dave_timeout` so the first frames are not sent in the clear; skipped when the ACK says DAVE version 0. The capture source starts only after that.
Decision: `update_video` matches the `updateVideo` IPC: op 12 with the new attributes and the new fps for the pump; the source is not restarted (the capture backend owns resolution changes).
Decision: pacing comes from the source (capture is real time); the pump sends as frames arrive with `send_*_frame_at`, pts rebased to the first frame of either track so audio and video share one timeline. Per track pts are strictly increasing (bumped by one RTP tick); a backward jump over 1 s is a source restart and rebases both tracks.
Decision: backpressure: a reader task drains the source channel into a bounded queue (8 video, 25 audio frames) and never blocks. A full video queue or a failed video send drops video up to the next keyframe and calls `request_keyframe` (at most once a second while waiting); audio overflow drops the oldest audio frame. Video before the first keyframe is dropped.
Decision: H265 E2E uses a committed VideoToolbox fixture replayed by a test source, regenerated with `DSTREAMY_H265_FIXTURE=<path> cargo test -p dstreamy-capture-macos --test videotoolbox write_h265_fixture -- --ignored`; openh264 is H264-only and an H265 software encoder would add a heavy dependency for a test.
Decision: the E2E tests count the mock's `dropped_frames` as accounted for. Root cause of the one frame it sometimes drops: davey shares one nonce sequence across audio and video and its per-user decryptor rejects nonces older than the first it decrypted; video goes through the pacer, so audio frame 0 can arrive first and video frame 0 is rejected. Reproduced in isolation; transport was bit-identical. Not a vendor or mock bug, so no fix commit; vault pitfall `davey-first-frame-nonce`.

Follow-ups for issue 08:
- Health is poll-only (`stats()` flags, `last_error` as a string); a status watch or a `closed()` future with a typed reason would let the supervisor react without polling.
- The handle owns the `MediaConnection` for its lifetime; reconnect and channel move (`set_voice_channel` + `create_stream`) currently mean `stop()` + `start_stream()`, which also restarts the source. Consider splitting the media pump from the Discord session so the connection can be swapped.
- The source is consumed and dropped on failure and in `stop()`; return it for reuse across retries.
- Viewer PLI is ignored by the vendored transport; wiring it to `request_keyframe` would also cover the lost-first-keyframe case above.
- The shared timebase offset shifts the other track's in-flight frames on a source restart; fine for restarts of both tracks, revisit if one track restarts alone.
