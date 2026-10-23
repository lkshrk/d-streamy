# 0004 — `dstreamy serve` with external producers, and an OBS output plugin

Status: proposed (2026-10-23). Amends 0001 (Windows support, external producers) and 0003 (the session lock applies to `daemon` too).

## Context

OBS Studio already captures and encodes what streamers want to show, and many stream to Twitch or similar at the same time. D-Streamy should take OBS's encoded H.264 and Opus and stream them to Discord Go Live, next to that normal stream, without re-encoding in Rust and without putting Discord code into OBS.

The existing Rust pieces fit: `Session` takes a `Box<dyn CaptureSource>`, the pump sends pre-encoded Annex B and Opus, and `dstreamy daemon` (ADR 0003) already exposes the commands over stdio. But `daemon` exits on stdin EOF and owns one parent, and a session ends when its source ends. An OBS restart must not end the stream.

External facts this ADR relies on are pinned in `docs/specs.md`. A behaviour with no source there is not relied on and not mocked: it is either measured in CI, or made configurable and measured once against real Discord.

## Decision

### Service

- **`dstreamy serve`** is a long-running process that exits only on `shutdown` or a signal. A client disconnecting never stops it or the session.
- **The command engine is shared.** The command handling moves out of `daemon.rs` into one engine that `daemon` (stdio, protocol 1, exits on EOF) and `serve` both drive. The target `external` is enabled only in `serve`. `update` on an `external` session is `invalid_params`, because the producer owns the encoder settings.
- **Producer settings drive the announced video.** A producer's `hello` declares codec, width, height and fps. The engine starts an `external` session with codec H.264, and when an attachment's values differ from what is announced it calls `Session::update_video`, so op 12 always matches the media. A producer whose values `Resolution` or `Fps` reject (odd or larger than 1920×1080; fps other than 15, 30 or 60) is refused with `incompatible`.
- **One account, one session.** The same token in two processes evicts the other (`StopReason::VoiceTakenOver`). Whichever of `stream`, `tui`, `daemon` or `serve` has a live session holds `File::try_lock` on `<config-dir>/session.lock`. The holder's PID goes in a separate `session.pid`, because an exclusive `LockFileEx` lock blocks reads on Windows. A second session fails with `already_running` and names the PID. This changes `daemon`: `start` can now fail with `already_running` because of another process. The error code already exists in protocol 1, so the wire format and fixtures don't change. The lock is released when the session state becomes `Stopped`, so a session that stops on its own frees it too. The lock is per config directory: two processes with different `--config-dir` values and the same token are not protected.

### External source

- **`ExternalSource: CaptureSource`** is fed by attachments. The session does not know about producers. Its frame receiver stays open across producer swaps and closes only when the grace period runs out (`[serve] grace_secs`, default 30) or the session stops. The grace period runs both while no producer is attached and while an attached producer has stalled (`[serve] stall_ms`, default 2000).
- **Attachments can come early.** The source exists from the control `start`, so a producer may attach before Discord is connected. Frames before `CaptureSource::start` are dropped. The grace period starts counting only at `CaptureSource::start`, because connecting can retry with backoff. `stop` followed by `start` on the same object keeps the attachment.
- **`ExternalSource` owns the timeline.** The pump's `Timebase` is created once per pump and is not involved in producer swaps. On a backward jump of more than 1 s it rebases each track separately, which is exactly what must not happen. So `ExternalSource` emits its own monotonic `u64` timeline, and the pump never sees a backward jump.
  - Producer pts are signed (OBS pts can be negative). The first pts of the first attachment maps to 0, and anything still negative is clamped.
  - A new attachment's first frames are held until both tracks have sent one (at most 500 ms; a single-track producer then goes ahead alone). One offset is then applied to both tracks: the smallest that keeps each track at or after its own last pts at the moment of detach plus the wall-clock time since. On a fresh timeline this maps the earlier first pts to 0. Taking the larger last pts minus the smaller first pts instead would push both tracks ahead of the clock by the producer's A/V skew on every reattach (about 21 ms with Opus priming), and that adds up. The pump stamps RTP from pts without wall-clock pacing, so per-track rebasing would shift audio against video by the length of a gap.
  - Idle frames follow the same clock and don't count towards the offset.
- **After a reattach, video resumes at a keyframe.** Audio resumes at once.
- **Idle between producers is a switch** (`[serve] idle`), because Discord documents nothing about idle Go Live:
  - `documented` (default): five Opus silence frames `F8 FF FE`, 20 ms apart, then nothing. Discord documents the five frames for voice connections; the spacing is the frame's own duration (RFC 6716 config 31). The frame is mono, which is valid in a stereo stream.
  - `none`: nothing.
  - `continuous`: a silence frame every 20 ms. Undocumented; kept as an experiment.

  A manual test against real Discord measures which one is needed and sets the shipped default.

### IPC

- **A local stream behind one trait**, in a new crate `dstreamy-ipc`. No auth tokens.
  - Linux and macOS: a Unix socket in a 0700 directory (`$XDG_RUNTIME_DIR/dstreamy/` on Linux, `$TMPDIR/dstreamy/` on macOS; a base directory or any of its parents that another user can write is rejected). When that is unset or the path is too long for `sun_path`, the fallback is a directory under the user's home (`~/.local/state/dstreamy/` on Linux, `~/Library/Application Support/dstreamy/` on macOS), never `/tmp`: in a world-writable directory another user could create the name first, block the service and impersonate it. Both sides check the directory (owned by us, mode 0700, not a symlink) and the peer's uid with `peer_cred`. The server drops other users before anything is read; the client refuses a server running as another user. A lock file in the directory covers the remove-stale-then-bind step, so two `serve` starting at once can't orphan each other.
  - Windows: the named pipe `\\.\pipe\dstreamy-<user SID>`, created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, a DACL for the current user only and remote clients rejected. As on Unix, both sides check each other: the server compares the client's token user (`GetNamedPipeClientProcessId`), and the client the server's (`GetNamedPipeServerProcessId`). The Windows details are provisional until that transport is built and tested.
- **The same wire format on every transport.** A connection starts with one JSON `hello` line naming its role.
  - `control`: NDJSON with the protocol 1 commands and error codes, plus `status` and `subscribe`, as `protocol: 2`. A client asking for a higher protocol gets `unsupported_protocol`. Lines are capped at 1 MiB; blank lines are skipped, and a trailing `\r` is accepted. A subscriber that falls behind is disconnected with `lagged`. The `hello` reply carries the connection's id as `connection`, which `status` reports as `started_by`; `lagged` is an error with `"id":null`.
  - `producer`: after `attached`, binary frames `u32 BE length | u8 kind | i64 BE pts_us | u8 flags | payload`. `length` counts everything after itself (the 10 bytes of kind, pts and flags plus the payload) and is at most 8 MiB. Kind 1 is an Annex B access unit, kind 2 an Opus packet, kind 3 end of stream; flag bit 0 marks a keyframe and is ignored on non-video frames (OBS sets `keyframe` on every Opus packet). The other flag bits are reserved and must be 0, and an end-of-stream frame has no payload; both are rejected otherwise. pts is signed because OBS pts can be negative (Opus priming). The service answers with NDJSON events: `keyframe_request`, `source`, and `detached` with a reason.
  - Golden fixtures pin both roles, and the Rust and C sides decode the same files.
- **Ingest is validated against the specs:**
  - Annex B start codes are required.
  - A keyframe carries an IDR slice and SPS and PPS.
  - pts equals dts, so no B-frames.
  - Opus packets total 20 ms. The per-packet stereo bit is not checked, because it is per packet.
  - A failure detaches the producer, and the session enters the grace period.

### OBS plugin

- **Written in C** from `obs-plugintemplate`, with the template's build raised to OBS 32.2.2. It holds no Discord, token or WebRTC code. Only its IPC client is platform-specific. It connects to a running `serve` and never spawns or owns it.
- **A separate output, next to the normal stream.** The plugin registers the encoded output `dstreamy_output` and starts and stops it itself, from:
  - a dock (`obs_frontend_add_dock_by_id`, one small C++/Qt file; the rest of the plugin is C) with Start/Stop, a server and voice channel picker, and the session state;
  - a Tools menu toggle;
  - an optional "start and stop with Start Streaming".

  OBS's stream service stays free for Twitch or any other service, and starting or stopping one never affects the other. Settings › Stream couldn't host a third-party service anyway: it offers only custom RTMP, the built-in services and WHIP.
- **Test control.** The plugin registers an obs-websocket vendor `dstreamy` with `start`, `stop` and `status` requests that run the dock's own code path and return its result and error text. obs-websocket's `StartOutput` is not enough: it needs the output to exist under a stable name, bypasses the dock and reports no failure. The vendor callback runs on a websocket worker thread and hands off to the Qt thread. `obs-websocket-api.h` (GPL-2.0, header-only) is vendored, because the template's OBS dependency build doesn't export it.
- **Encoders:**
  - Audio is always the plugin's own `ffmpeg_opus` encoder (960-sample frames), because the stream service usually needs AAC.
  - Video defaults to the plugin's own H.264 encoder, bound to the canvas (`obs_encoder_set_video`; audio with `obs_encoder_set_audio`): no B-frames, `keyint_sec` 2, scaled down to fit 1920×1080 with even dimensions on the GPU (`obs_encoder_set_gpu_scale_type`, so texture encoders like NVENC and VAAPI keep their GPU path). The frame rate is 15, 30 or 60 via `obs_encoder_set_frame_rate_divisor`: a 60 fps canvas allows 60, 30 or 15, a 30 fps canvas 30 or 15. 24, 25, 50 and NTSC (30000/1001) canvases have no valid divisor and are reported with the setting to change.
  - Optionally the stream's video encoder is shared, if it meets the same rules: H.264, B-frames explicitly off, `keyint_sec` in [1, 2], even size up to 1920×1080, FPS 15, 30 or 60, and an empty custom-options string (`x264opts` for x264, `opts` for NVENC, `ffmpeg_opts` for VAAPI), because those override every other key and x264's `annexb=0` would break Annex B. Otherwise the plugin falls back to its own encoder, or errors if the user chose that.
  - Sharing is only possible while the main stream is active. libobs keeps the two outputs independent (each has its own callback and waits for its own first keyframe), but the frontend owns the encoder: it has no video until streaming was set up once, it rebuilds encoders on a Settings Apply, and it changes the encoder's bitrate while streaming. So share mode follows `OBS_FRONTEND_EVENT_STREAMING_STARTING` and `_STOPPING` (STARTED fires only after the remote connect, and never on a failed one) and switches to the plugin's own encoder when the main stream stops. One encoder error stops both outputs.
  - A runtime guard rejects any packet with pts ≠ dts.
- **The OBS threads are never blocked.**
  - `encoded_packet` runs on the encoder thread, under mutexes shared with audio and with every output fed by the same encoder. In both modes a blocking call stalls the user's main stream too (the audio encoder or, in share mode, the video encoder is shared). It only copies the packet into a bounded ring and returns; a writer thread does the IPC. When the ring is full, video is dropped until the next keyframe and the oldest audio is dropped.
  - Start runs the connect, `hello` and control `start` on a thread, as OBS's RTMP output does.
- **SPS/PPS** are taken from the encoder's extra data and prepended to any keyframe that lacks them. Extra data is read lazily, because NVENC, VAAPI and VideoToolbox only provide it after their first packet. x264 repeats them in-band by default, but `x264opts` can turn that off, so the plugin never relies on it. Extra data in avcC form (possible from third-party encoders) is rejected.

### Keyframe requests

libobs has no public API to request a keyframe from an encoder. Some encoders (NVENC, AMF) emit an IDR as a side effect of a settings update; that is per encoder, undocumented as an interface, and would disturb a shared stream, so it is not used. That leaves every keyframe request unanswered, not only Discord's:
- viewer PLI and FIR, surfaced by our transport;
- the pump's own requests: on every attach after a reconnect or channel move, 500 ms after DAVE comes up (davey may reject the first keyframe), and on queue overflow or a lost frame.

They are logged, and the `keyframe_request` event still reaches the producer so a future encoder that can honour it may. A keyframe interval of at most 2 s bounds recovery, as the reference client's 1 s interval does.

For PLI this is conformant, since RFC 4585 makes a reaction optional. For FIR it deviates from RFC 5104 §4.3.1.2, which requires a refresh point. This is accepted deliberately.

### Supervision

`dstreamy service install|uninstall|status`, user-level, no root or admin:
- **Linux:** a systemd user unit with `Restart=on-failure` and `RestartSec=2`.
- **macOS:** a launchd agent with `KeepAlive {SuccessfulExit: false}`, `RunAtLoad`, and `ThrottleInterval` 2 (the default is 10 s).
- **Windows:** a Task Scheduler logon task with `LogonType=InteractiveToken`, `ExecutionTimeLimit=PT0S`, no battery restrictions, and `IgnoreNew`. Whether `RestartOnFailure` restarts a killed process is undocumented. If a test shows it doesn't, `serve --supervise` respawns the service itself.

A service restart ends the Discord session; no session state survives process death.

### Platforms

The work ships in a hard sequence: Linux first, then macOS, then Windows. Each platform is complete in CI before the next starts. On Windows, `dstreamy` streams only from an external producer and the test pattern; no native capture backend is added.

Unix-only code already exists behind `cfg(unix)`: the 0600 token and config files (`config.rs`), logging, and signal handling (`signals.rs`). Windows gets equivalents: an ACL that limits the token and config files to the current user, and Ctrl-C, close and shutdown events that stop `serve` like SIGTERM. New Unix-only code stays inside `dstreamy-ipc`, the plugin's IPC module and the service installers.

## Consequences

- OBS can stream to Discord and to Twitch at the same time, and either can stop independently.
- The directory trust check reads owner and mode bits only. On macOS, an ACL on a parent directory can grant another user write access without changing the mode; only the owner or root can set it, and `~/Library` carries a standard deny ACL, so refusing every ACL would break the home fallback. This is accepted.
- Flatpak OBS can't reach the service's socket in its default sandbox, so the first release supports native OBS packages only; Flatpak needs a later decision on a sandbox-visible socket path or a documented override.
- An OBS crash or restart within the grace period keeps the Discord stream; the viewer sees a short freeze, then picture from the next keyframe.
- The default own-encoder mode costs a second video encode, and a hardware encoder may hit the driver's session limit. Share mode avoids that but works only while the main stream runs and constrains its encoder; a Twitch encoder error ends both streams.
- Picture recovery takes up to the 2 s keyframe interval after viewer loss, a reconnect, a channel move or a DAVE transition, because no keyframe request can be honoured; up to two intervals if davey rejects the first keyframe.
- Each new producer attachment re-announces its size and frame rate, so a restarted OBS with a different canvas is shown correctly.
- If a new producer's audio/video skew is smaller than the previous one's, both tracks start that much ahead of the clock, once, because one shared offset can't move either track backwards. OBS's skew comes from fixed encoder priming, so reconnects with the same settings don't trigger it.
- `daemon`'s wire format and protocol 1 fixtures are unchanged; `start` can additionally fail with `already_running` when another process holds the session lock. The macOS app keeps working as is.
- The idle and grace defaults are provisional until a manual test against real Discord measures them.
- OBS builds from source in CI on every platform, because no prebuilt package matches the pinned version. A CI spike showed libobs runs headless on the Linux, macOS and Windows runners (software rendering; Linux needs `xvfb-run`), and the full OBS frontend with obs-websocket starts headless too, so the plugin is tested against real OBS on all three.

## Rejected

- **OBS as the stream service** (a custom service on the `DSTREAMY` protocol, selected from the Tools menu). It occupies the only stream service slot, so the user can't stream to Twitch at the same time, and saving Settings › Stream replaces it.
- **Discord transport inside the OBS plugin.** It would duplicate the DAVE, WebRTC and gateway code in C or C++, tie the stream's life to OBS's process, and leak the token into OBS.
- **RTMP or another media server between OBS and `dstreamy`.** It adds a hop, a protocol and its latency for no gain over a local socket.
- **Per-track timestamp rebasing.** It desyncs audio and video by the gap length, because the pump does not pace by wall clock.
- **A token or password on the local socket.** It adds secret handling without adding protection beyond the per-user directory, the DACL and the peer check.
