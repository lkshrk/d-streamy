# Review round 2 (2026-10-21)

Five read-only reviewers. No high findings. Owners: A = discord, B = vendor+mock, C = capture+media, D = cli+core+CI.

## A — dstreamy-discord
- MEDIUM session.rs:566: a lagged gateway broadcast is only logged, so a VoiceStateUpdate or Ready can be lost (stale channel or stream key). Send own-user VOICE_STATE_UPDATE and READY over an ordered mpsc, or treat Lagged as resync.
- MEDIUM session.rs:513: `MediaPump::start` (CaptureSource::start) runs inline in select with no timeout; a hung start blocks stop and shutdown forever. Use a timeout or a droppable phase future.
- LOW pump.rs:194: attach races send_frames; a popped delta can be the first frame on a new connection before the IDR. Swap the connection under the queue lock.
- LOW gateway/connection.rs:395 (+:316): a huge heartbeat_interval overflows Instant and panics. Clamp it.
- LOW gateway/connection.rs:421: op 7 before READY counts as healthy, so it loops forever. Use `healthy: *ready`.
- LOW gateway/inflate.rs:23: the compressed buffer is unbounded until the suffix. Cap it at MAX_MESSAGE.
- MEDIUM tests/session.rs:456: the keyframe gap uses a wall-clock `streaming_at`, and overflow requests count. Count DAVE-triggered requests only and assert order.
- MEDIUM tests/session.rs:427,435-436: per-snapshot `bitrate>0` and mean fps/audio rates are wall-clock. Assert invariants or use paused time.
- MEDIUM tests/stream.rs:316-319: mean fps = sent/wall time is wall-clock. Check pts spacing or sent+dropped vs the source span.
- LOW tests/gateway.rs:316-318: binds port 0, drops it, then dials it, racing parallel binds. Keep the listener bound without accepting, or use 127.0.0.1:1.

## B — vendor discord-stream-rs + webrtc patch + mock
- MEDIUM vendor dave.rs:134: an empty connected_users passes None, which skips davey's UnexpectedUser check. Always pass Some (verify against the JS reference via `npm pack @dank074/discord-video-stream` if it isn't in node_modules).
- MEDIUM mock sink.rs:62 / voice.rs:341: the RTP, op 5 and op 12 SSRCs are not checked against READY. Enforce them.
- LOW mock voice.rs:360: SELECT_PROTOCOL codecs and protocol are ignored. Honour them, and fix the vault pitfall text.
- LOW mock sink.rs:157: plaintext frames are accepted while DAVE is active. Record them as dropped.
- LOW mock voice.rs:325: heartbeats are not enforced (4009, seq_ack). Enforce them.
- LOW vendor webrtc.rs:280: the H264 offer has no fmtp; libdatachannel sends `profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1`. Match it, or record a Decision if the mock partial-match pitfall blocks it.
- LOW vendor voice/connection.rs:351: `dave_channel_id(..).unwrap_or(0)`; JS throws. Return an error.
- LOW comments: vendor/webrtc track_local/mod.rs:130-131, dave.rs:228-229, :234-235, processing/annexb_rw.rs:26-27, processing/sps_vui.rs:11-15. Cut each to one line.
- MEDIUM mock tests/self_test.rs:134: zero dropped frames over UDP with 2 ms spacing is load-sensitive. Pace by pts, or bound the loss.

## C — capture-macos, capture-linux, media
- MEDIUM capture-macos source.rs:375: keyframe requests are only served on Complete frames, and a static screen sends none, so video stays black after re-attach. Re-encode the last retained pixel buffer with ForceKeyFrame on request and every ~2 s idle.
- MEDIUM capture-linux source.rs:73: the portal session closes only on a clean close; Drop, the stop timeout and cancelled start leak it. The OwnedFd is dropped while spawn_blocking uses the raw fd. Move the fd into the closure, and add a Drop guard that spawns session.close().
- LOW capture-linux pipeline.rs:540: probe candidates share tx while audio is already PLAYING, so audio pts go backwards and audio errors are blamed on the encoder. Use a channel per candidate, or probe video first, and tell bus errors apart by source.
- LOW capture-linux source.rs:126: a spent restore token is kept when the portal returns none. Assign it unconditionally.
- LOW capture-linux pipeline.rs:442: the crop isn't validated against the source size; failure shows as an encoder error after 4×3 s. Validate it before the fallback.
- LOW media h264.rs:68: split_at panics on short I420 data. Return MediaError.
- LOW comments: media annexb.rs:15 (3 lines); vt.rs:387-388 borderline.
- MEDIUM capture-macos tests/videotoolbox.rs:126-131: HEVC "skipping" passes on CI. Fail under CI on Apple Silicon, or add a MANUAL-CHECKLIST line.

## D — cli, core, CI/release
- MEDIUM cli tui/mod.rs:255: a refutable `Some(Input::Key(Key::Interrupt))` in select! disables the branch on any other key. Loop inside the branch.
- LOW core logging.rs:74,87: the log file is 0644 and can hold stream keys at -vv. Create it 0600.
- LOW core config.rs:157: fixed `.tmp` without O_EXCL/NOFOLLOW, which races. Use tempfile::NamedTempFile::new_in + persist.
- LOW core config.rs:68: the config crop isn't validated. Validate it in load/save like parse_crop.
- LOW cli main.rs:36: hidden --gateway-url accepts any host or ws://, which could leak the token. Reject a non-loopback non-default URL.
- LOW cli main.rs:176: blocking stdin/rpassword inside async. Read it before block_on.
- LOW cli tui/mod.rs:57: tui always exits 0. Exit 1 on connect failure or an unrequested stop (match `stream`).
- LOW core dead/duplicate code: CaptureDimensions (dup in capture-macos), CaptureMetrics, AudioFrameBuffer, CropRect::apply_bgra, StreamHealth::overall_level, RotatingFileWriter::path/rotated_path; CropRect/CaptureTarget are duplicated between core and media. Delete or unify.
- LOW cli stream.rs:226 vs tui/model.rs:324 and capture.rs:29 vs model.rs:397: duplicated level/kind labels and start/teardown. Add shared helpers.
- MEDIUM cli tests/cli.rs:481: the second-SIGINT test accepts 0 or 130. Slow the mock shutdown, then assert 130.
- LOW cli tests/cli.rs:505-511: the 4 s wall-clock bound. Assert a single connection instead.
- LOW MANUAL-CHECKLIST: add the ignored `targets_list_macos_displays` test.
- MEDIUM rust-toolchain.toml: `stable` is unpinned with -D warnings. Pin 1.NN.0 and install from the file.
- MEDIUM workflows: actions are pinned by tag or branch (dtolnay@stable). Pin full SHAs with version comments, and add dependabot for github-actions.
- MEDIUM ci.yml:53-54,77-78 and linux.sh:7: no `--locked`. Add it.
- LOW ci.yml: no top-level `permissions: contents: read`.
- LOW ci.yml:57: ubuntu-latest. Pin ubuntu-24.04.
- LOW release-rust.yml:38: check matrix lacks ubuntu-24.04-arm.
- LOW release-rust.yml:131: release create isn't idempotent. Use view || create, then upload --clobber.
- LOW release.yml:22: Rust jobs gate the Swift workflow_run release. Move the rust jobs to their own workflow (rust-ci.yml).
- LOW Formula: hashes are maintained by hand. Have the release job emit a filled formula in the step summary.
