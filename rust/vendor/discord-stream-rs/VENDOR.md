# Provenance

This directory started as the published crate `discord-stream-rs` 0.1.3 by Tky567
(MIT, see `LICENSE`; upstream https://github.com/Tky567/stream-discord-rs, commit
`50a402c3da9f14755ef622f27387db502d8300f9`, `.crate` SHA-256
`228ddf36e24d1826c6f93d60b9cc0fb9def2639f28a08063c9bf493d454cc15c`). It is now a
fork maintained inside D-Streamy: the version carries the `+dstreamy` build metadata,
the crate is `publish = false`, and upstream is not synced. `README.md` is upstream's
and describes the original crate, not this fork (FFmpeg is opt-in here, so
`LIBCLANG_PATH` is only needed with the `media` feature).

## Local changes

Measured against the import commit `4e0f597`, the fork differs by about 7200
insertions and 3600 deletions across 33 files. The commits, oldest first:

| Commit | Change |
|---|---|
| `46e4974` | build(vendor): make FFmpeg opt-in and add discord-stream-rs to the workspace |
| `400c50b` | style(vendor): rustfmt and clippy cleanups |
| `b36d26a` | fix(vendor): send client voice binary opcodes without a sequence prefix |
| `7801232` | fix(vendor): advertise max_dave_protocol_version in voice IDENTIFY |
| `315dd8e` | fix(vendor): follow the JS DAVE transition semantics |
| `2ae9eeb` | fix(vendor): make the SPS VUI rewrite fail-safe |
| `684e30b` | fix(vendor): bring the WebRTC transport to parity with the JS reference |
| `a01cea0` | fix(vendor): implement the voice/stream connection lifecycle and StreamConnection |
| `1820e60` | fix(vendor): complete the main gateway opcode list |
| `cdee276` | fix(vendor): only accept H264 and H265 packetizers |
| `4c462c1` | fix(vendor): make PendingConnection::ready cancel-safe |
| `fa94b91` | fix(vendor): parse scaling lists per spec and widen Exp-Golomb writes |
| `a33107b` | fix(vendor): harden voice reconnects, heartbeats and video queueing |
| `44e7aba` | feat(mock-discord): in-process Discord gateway, voice, WebRTC and DAVE (test hooks) |
| `0a1c398` | feat(vendor): add MediaConnection::ptr_eq |
| `f5b44f0` | fix(vendor): surface viewer PLI and FIR as keyframe requests |
| `aa35cfd` | fix(vendor): use rustls for the voice WebSocket instead of native-tls |
| `542702b` | docs(vendor): cut multi-line doc comments to one line |
| `5b8621c` | fix(vendor): always check DAVE proposals against connected users |
| `d7f2265` | fix(vendor): reject channel ids that give no DAVE group id |
| `c43f9ea` | feat(vendor): offer libdatachannel's default H264 fmtp |
| `181377c` | test(mock): enforce READY SSRCs, SELECT_PROTOCOL, heartbeats and DAVE |
| `ed0e2c7` | feat(vendor): offer libdatachannel's default Opus fmtp |
| `e2f4b74` | refactor(obs-packet): move the packet conversion to a Rust crate |
| `89f09a2` | fix: bound packet parsing and refuse test features in release builds |
| `a0dde6b` | fix(transport): DAVE-encrypt video when the pacer sends it |
| `0756ba6` | fix(transport): count paced video once, at the outcome |
| `6acb700` | fix(stream): flush stale pacer video when a frame is lost |
| `e5043dd` | fix(stream): apply pacer progress once per connection and bound stop |
| `bfe4e1d` | fix(stream): close keyframe-loss and pacer-stop gaps from review |
| `635e095` | fix(stream): rate-limit keyframe re-requests outside a full pacer |
| `c132473` | test(stream): pin lag recovery and nonce order without timing |
| `f438994` | fix(transport): pace video by deadline, not a fixed 1 ms sleep |
| `2a782d1` | fix(transport): bound the burst after idle from when packets go out |
| `38fd691` | fix(transport): keep the idle burst limit past a dropped frame |
| `479b566` | fix(test): give a DAVE keyframe longer than 5 s to reach the wire under load |

Later changes are listed by `git log --oneline 4e0f597..HEAD -- rust/vendor/discord-stream-rs`;
add them here when touching this file.
