# External specifications

Every external behaviour `dstreamy serve`, the IPC layer and the OBS plugin depend on, with the pinned source it is implemented from. A behaviour not listed here has no source and must not be relied on or mocked. Rows marked *measured* come from CI runs of `obs/spike/headless.c` (removed) or `obs/harness/obs-harness.c` with the traces under `obs/tests/traces`; rows marked *undocumented* are deliberately handled as configurable or tested in CI.

## Pins

| Name | Pin |
|---|---|
| OBS | `obsproject/obs-studio` tag `32.2.2`, commit `ba2f32bdf791005443988a4955e963663e16b1ed` |
| obs-websocket | `5.7.4`, `obsproject/obs-websocket@1ef34bf48110c2a18184e50e41cd0b1a855e2147` (submodule at the OBS tag), RPC version 1 |
| obs-plugintemplate | `obsproject/obs-plugintemplate@3e7d7ac3b5342cd7d9b88890b9c70b472d1520fc`, with `buildspec.json` raised to OBS 32.2.2 (no template commit targets 32.x) |
| obs-deps | `2026-07-15` (from the OBS tag's `buildspec.json`) |
| libx264 | `mirror/x264@c24e06c2e184345ceb33eb20a15d1024d9fd3497` |
| FFmpeg | the version obs-deps `2026-07-15` ships; avcC description only from `FFmpeg/FFmpeg@c4c791c2cd02e4e50a5644e868c2a44950cbddc7` |
| Discord API docs | `discord/discord-api-docs@aaef8ae4c3398fcdabeec0d1dd5a3e69be587ad8` |
| Discord reference client | `@dank074/discord-video-stream` 6.0.0, `dank074/Discord-video-stream@b1d688ddc5d3642a8a571142f2eba1d181b71fd6` |
| H.264 | Rec. ITU-T H.264 (V16) 06/2026, `T-REC-H.264-202606-I` |
| avcC | ISO/IEC 14496-15:2024 (paywalled). Public description: `FFmpeg/FFmpeg@c4c791c2cd02e4e50a5644e868c2a44950cbddc7:libavformat/avc.c:111-131` |
| AAC AudioSpecificConfig | ISO/IEC 14496-3 (paywalled). Public description: `FFmpeg/FFmpeg@c4c791c2cd02e4e50a5644e868c2a44950cbddc7:libavcodec/mpeg4audio.c:77-101` (5-bit object type, 4-bit sampling index, 4-bit channel configuration), `libavcodec/mpeg4audio_sample_rates.h:30-33` (index 3 = 48 kHz), `libavcodec/mpeg4audio.h:75` (`AOT_AAC_LC` = 2) |
| Opus | RFC 6716 (updated by RFC 8251), RFC 7587 |
| RTCP feedback | RFC 4585, RFC 5104 |
| Rust | 1.99.0 (`rust/rust-toolchain.toml`) |
| tokio | 1.54.0 (`rust/Cargo.lock`) |
| WebSocket | RFC 6455; `tungstenite` 0.24.0 (`rust/Cargo.lock`) |
| Win32 docs | `MicrosoftDocs/sdk-api@656e9656bfaef34dd29cd7ee9e1146aec17e924c`, `MicrosoftDocs/win32@ca8274decead49403d70ceeed4af8d4ca74eaf11` |
| systemd | v255 (ubuntu-24.04 ships 255.4), `systemd/systemd@db11bab38ccf1ed257f310d29070843d4c58ea01` |
| Linux man-pages | 6.19 |
| macOS man pages | Xcode 27.2 RC 1 man pages (`keith/xcode-man-pages@cb44759ff0c3f25182b2eeeb5d44ca0de9f3c01a`); Apple OSS `Libc-1752.120.2`, `launchd-842.92.1` |
| Flathub OBS | built from `build-aux/com.obsproject.Studio.json` in obs-studio at the OBS tag (the separate `flathub/com.obsproject.Studio` repository is stale since OBS 27.1.3), plus the Flatpak 1.16.1 sandbox-permissions docs |
| cargo-fuzz | 0.13.2 on `nightly-2026-07-20` |
| obs-multi-rtmp | 0.7.4.4, `sorayuki/obs-multi-rtmp@81006873b34dfe6524f671f2551ef162156a4f47` (GPL-2.0; patterns only, no code taken) |
| MediaMTX | v1.21.2, `bluenviron/mediamtx@914e39535ece709919ac325aaeed98d394ed9093` (local RTMP receiver in tests) |

## OBS

| Behaviour | Source | Used by |
|---|---|---|
| Encoded A/V output: `obs_output_info` with `OBS_OUTPUT_AV \| OBS_OUTPUT_ENCODED`; `get_name`, `create`, `destroy`, `start`, `stop` and `encoded_packet` required | `libobs/obs-output.h:25-88`, `libobs/obs-module.c:1061-1113` | plugin output |
| `encoded_video_codecs` and `encoded_audio_codecs` are only reported, never enforced; the output checks `obs_encoder_get_codec` itself | `libobs/obs-output.c:3253-3304` | plugin output |
| Start sequence: `obs_output_can_begin_data_capture`, `obs_output_initialize_encoders`, a connect thread, then `obs_output_begin_data_capture` | `plugins/obs-outputs/rtmp-stream.c:1540-1551,1088` | plugin start thread |
| OBS's built-in H.264 encoders hand `encoded_packet` Annex B, with Annex B SPS+PPS as extra data. libobs also accepts extra data without a start code (avcC) and passes it through, so a third-party encoder may supply avcC | `libobs/obs-avc.c:68-104,219-267` (no-start-code branch `:231-234`), `rtmp-stream.c:798-807,1866` | packet conversion: detect avcC extra data and reject it |
| x264 repeats SPS/PPS on every keyframe by default (libx264 `b_repeat_headers=1`). The user's `x264opts` string is passed to `x264_param_parse`, so `repeat-headers=0`, `global-header=1`, `bframes=` or `keyint=` override it and bypass the `bf` and `keyint_sec` keys | `plugins/obs-x264/obs-x264.c:288,544,565-569`, `x264 common/base.c:471-472,1380-1383`, `x264 encoder/encoder.c:3663-3666` | packet conversion (always prepend when missing); share-mode validation rejects a non-empty `x264opts` |
| NVENC, VAAPI and VideoToolbox expose extra data only from the first packet or keyframe | `plugins/obs-nvenc/nvenc.c:1116-1130`, `plugins/obs-ffmpeg/obs-ffmpeg-vaapi.c:651-680`, `plugins/mac-videotoolbox/encoder.c:1004-1011` | packet conversion (lazy) |
| No public keyframe-request API for encoders. An IDR is only reachable as a side effect of reconfiguring some encoders (NVENC sets `forceIDR`, AMF `force_idr` on update) | `libobs/obs-encoder.h:35-41,112`, `plugins/obs-nvenc/nvenc.c:115-119`, `plugins/obs-ffmpeg/texture-amf.cpp:1450` | keyframe policy (side effect not used, ADR 0004) |
| libobs drops video until the first keyframe, per output and per callback | `libobs/obs-output.c:2237-2245`, `libobs/obs-encoder.c:1334-1336` | plugin, tests |
| `encoder_packet`: `pts` and `dts` are `int64_t` in units of `timebase_num/timebase_den`, both `int32_t`; `keyframe` is a `bool`, and `dts_usec` is libobs's own dts in µs | `libobs/obs-encoder.h:100-121` | packet conversion (timebase → µs, refuse a timebase ≤ 0) |
| `encoded_packet` runs synchronously on the encoder thread, under the encoder's callback mutex and the output's interleaver mutex. Blocking stalls both tracks, and every other output fed by the same encoder (for example a shared Twitch stream) | `libobs/obs-encoder.c:1439-1447`, `libobs/obs-output.c:1756,2235-2306` | ring buffer design, share mode |
| A `NULL` packet means an encoder error; the output calls `obs_output_signal_stop` | `libobs/obs-encoder.c:1380-1382`, `docs/sphinx/reference-outputs.rst:142-150` | plugin output |
| B-frame and keyframe settings keys and defaults per encoder (x264 `bf` only applied when user-set, `veryfast` uses B-frames; QSV and VT use `bframes`; VT `keyint_sec` defaults to 2) | `obs-x264.c:429-430`, `obs-nvenc/nvenc-properties.c:47`, `obs-ffmpeg-vaapi.c:865-866`, `texture-amf.cpp:1231,1461-1471`, `obs-qsv11.c:181,459-464`, `mac-videotoolbox/encoder.c:1384-1388` | settings validation |
| `ffmpeg_opus` encoder id; 20 ms (960-sample) frames by default; first pts negative because of encoder priming (measured: -312) | `plugins/obs-ffmpeg/obs-ffmpeg-audio-encoders.c:482` (id), `:494` (`get_priming_samples` = `enc_initial_padding`); FFmpeg `libavcodec/libopusenc.c` `frame_duration` default 20.0; measured frame size 960, timebase 1/48000 | audio encoder |
| `color_source` resolves to v1 (400×400); use `color_source_v3` | `plugins/image-source/color-source.c:133,149,166` | test harness |
| Audio packets from `ffmpeg_opus` always have `keyframe = true` | `plugins/obs-ffmpeg/obs-ffmpeg-audio-encoders.c:394` | producer flag rule |
| Plugin install paths and `OBS_PLUGINS_PATH` / `OBS_PLUGINS_DATA_PATH` | `frontend/widgets/OBSBasic.cpp:120-180`, `libobs/obs-nix.c:52-62` | packaging, tests |
| Flatpak OBS cannot reach `$XDG_RUNTIME_DIR/dstreamy/` by default: `--filesystem=host` excludes `/run`, and only `pipewire-0` is shared | `build-aux/com.obsproject.Studio.json` at the OBS tag; Flatpak 1.16.1 `flatpak-run(1)` "--filesystem" | Flatpak support (not supported in the first release) |
| Build presets: `ubuntu-ci` (Ninja), `macos-ci` (Xcode), `windows-ci-x64` (Visual Studio 18 2026) | `CMakePresets.json`, `.github/workflows/build-project.yaml` | CI |
| libobs runs headless on the CI runners: Linux OpenGL/EGL on Mesa llvmpipe under `xvfb-run` (`obs_startup` needs an X display for hotkeys), macOS OpenGL or Metal software rendering, Windows D3D11 on WARP | measured: `obs/spike/headless.c`, CI run `lkshrk/d-streamy` 39995038738 at `4e88736`; Linux also by `obs-harness`, CI run 40028252651 | harness, UI tests |
| A non-Qt host must allow-list modules (`obs_add_safe_module`); frontend plugins such as obs-websocket abort without a Qt application | measured, same run | harness |
| Shutdown needs `obs_source_remove` on every source and a drained destroy queue before `obs_shutdown`, or macOS crashes | measured, same run | harness |
| Packets measured at 32.2.2: H.264 Annex B in packets and extra data; x264 repeats SPS/PPS on every keyframe; `bf=0` gives pts = dts, timebase 1/fps; Opus 960-sample frames, timebase 1/48000, first pts -312 (priming) | measured: `obs/tests/traces/linux-*` (provenance in each `PROVENANCE.md`), CI run `lkshrk/d-streamy` 40028252651; first seen in spike run 39995038738 | packet conversion, timeline |
| x264 with `bf` unset (preset `veryfast`) or `bf=2` uses B-frames: pts ≠ dts and the first dts is negative (measured -2) | `plugins/obs-x264/obs-x264.c:112,429-430`; measured: traces `linux-x264-main-bf{2,unset}-keyint2-30fps`, CI run 40028252651 | settings validation, negative traces |
| x264 scene-cut detection is on by default (threshold 40) and only `ultrafast` clears it, so a scene change forces an IDR regardless of `keyint_sec`; `x264opts` (`scenecut=0`) is applied after `keyint_sec` and turns it off. `keyint_sec=0` leaves x264's default `keyint` of 250 frames | x264 `common/base.c:385,388,499,1092-1096`; `plugins/obs-x264/obs-x264.c:112,405-406,493-494`; measured: traces with `scenecut=0`, CI run 40028252651 | harness, keyframe tests |
| `ffmpeg_opus` first packet pts is minus the encoder pre-skip, and the extra data is a 19-byte `OpusHead` carrying that pre-skip (312) | `plugins/obs-ffmpeg/obs-ffmpeg-audio-encoders.c:494`; measured: trace `linux-ffmpeg_opus-defaults`, CI run 40028252651 | timeline |
| `ffmpeg_aac` frame size is the codec's `frame_size`, 1024 when the codec gives none; its extra data is an AAC AudioSpecificConfig (measured AAC LC, 48 kHz, stereo) | `plugins/obs-ffmpeg/obs-ffmpeg-audio-encoders.c:142-144`; layout per the AAC AudioSpecificConfig pin; measured: trace `linux-ffmpeg_aac-defaults`, CI run 40028252651 | negative trace (AAC refused) |
| `obs_reset_audio2` sets `max_buffering_ms` and `fixed_buffering`; with fixed buffering libobs buffers the full maximum up front instead of growing from 0. The frontend's low-latency option uses 20 ms fixed. With the default dynamic buffering a source chunk that arrives after its mix tick is mixed as silence (measured on a loaded CI runner) | `libobs/obs.c:1615-1621`, `libobs/obs-audio.c:670-673`; frontend `frontend/widgets/OBSBasic.cpp:1696-1702` (`max_buffering_ms = 20` at `:1699`); measured: CI run 40022796948 (dropout), fixed by 200 ms fixed buffering | harness audio |
| Tick callbacks (`obs_add_tick_callback`) run on the graphics thread before the sources tick, and a video source's `obs_source_update` is deferred to its next video tick, so an update made in a tick callback renders in that same frame | `libobs/obs-video.c:47-53` (callbacks) vs `:76` (source ticks); `libobs/obs-source.c:1073-1074` (deferral), `:1373-1374` (applied in tick) | harness flash marker |
| `obs_get_video_frame_time` is the graphics thread's frame grid: set once at thread start, then advanced by whole frame intervals (several when frames lag) | `libobs/obs-video.c:1172`, `:809-822`; `libobs/obs.c:2882` | harness markers |
| Async audio from `obs_source_output_audio` whose timestamp is within `MAX_TS_VAR` (2 s) of `os_gettime_ns` is taken as a direct timestamp | `libobs/obs-source.c:1584`, `libobs/obs-internal.h:1060` | harness audio source |
| macOS runner (`xcode-27`): a GUI session (`gui/501`, Aqua) exists; VideoToolbox `…ave.avc` H.264 works, the software `…videoencoder.h264` fails to start | measured, same run | launchd test, VT traces |
| Settings › Stream offers only `rtmp_custom`, `rtmp_common` and WHIP; a third-party service can't be listed | `frontend/utility/BasicOutputHandler.cpp:179-222` (`GetStreamOutputType`) | why D-Streamy is a separate output |
| Dock: `obs_frontend_add_dock_by_id(id, title, QWidget*)`; docks are added hidden and floating | `frontend/api/obs-frontend-api.h:155` | plugin dock |
| Streaming events: `STREAMING_STARTING` before the output starts, `STREAMING_STARTED` only after the remote connect (never on a failed connect), then `STOPPING`/`STOPPED` | `frontend/widgets/OBSBasic_Streaming.cpp:89,267` | share mode, "start with Start Streaming" |
| One encoder may feed several outputs: each output adds its own callback, gets the same packets and waits for its own first keyframe; stopping one doesn't reset the encoder for the other; an encode error stops every output on that encoder | `libobs/obs-encoder.c:809-846` (start), `:848-890` (stop), `:1326-1369` (per-callback first keyframe), `:1439-1447` (packet loop); precedent `frontend/utility/AdvancedOutput.cpp:75,372-375` | share mode |
| The frontend owns the stream encoder: it has no video until streaming is set up, a Settings Apply with no active output recreates the output handler and its encoders, and it updates the encoder's settings (bitrate) while streaming | `frontend/widgets/OBSBasic_OutputHandler.cpp:26-33` (`ResetOutputs`), `frontend/utility/AdvancedOutput.cpp:268,336` (`obs_encoder_update`) | share mode only while the main stream is active |
| A new encoder has no media until `obs_encoder_set_video` / `obs_encoder_set_audio` | `libobs/obs.h:2413,2416`; reference use `sorayuki/obs-multi-rtmp@81006873b34dfe6524f671f2551ef162156a4f47:src/push-widget.cpp:192,216` | own encoder |
| `obs_encoder_set_scaled_size`, `obs_encoder_set_gpu_scale_type` (default `OBS_SCALE_DISABLE` = CPU), `obs_encoder_set_frame_rate_divisor`; init-only settings are refused while the encoder is active | `libobs/obs.h:2290-2304`, `libobs/obs-encoder.c:937-943,995-1040`; in-tree example `frontend/utility/MultitrackVideoOutput.cpp:162-181` | own encoder |
| NVENC `opts` (`frameIntervalP`, `gopLength`, `idrPeriod`) and VAAPI `ffmpeg_opts` (`bf`, `g`) override the B-frame and keyframe keys; x264 `x264opts` can also set `annexb=0` | `plugins/obs-nvenc/nvenc-properties.c:27-28`, `plugins/obs-nvenc/nvenc-opts-parser.c:92-93,130`, `plugins/obs-ffmpeg/obs-ffmpeg-vaapi.c:345-351`, `plugins/obs-x264/obs-x264.c:279-291` | share-mode validation |

## obs-websocket

| Behaviour | Source | Used by |
|---|---|---|
| `GetStreamServiceSettings`, `SetStreamServiceSettings`, `GetStreamStatus`, `StartStream`, `StopStream` | `docs/generated/protocol.md:3247,3264,5366,5405,5415` | UI end-to-end tests |
| `StartOutput`/`StopOutput` act on an existing output by name only and ignore `obs_output_start`'s result | `src/requesthandler/RequestHandler_Outputs.cpp:316-425` (result ignored at `:384-398`) at obs-websocket 5.7.4 | why tests use a vendor request |
| `obs-websocket-api.h` is header-only (GPL-2.0, API version 3) and exported only when OBS builds its plugins, which the template's dependency build does not | `lib/obs-websocket-api.h:25`; `cmake/obs-websocket-api.cmake`; obs-studio `plugins/CMakeLists.txt:25-29` | vendored header |
| Vendor API: `obs_websocket_register_vendor` from `obs_module_post_load`, `obs_websocket_vendor_register_request`, client `CallVendorRequest`; callbacks run on a websocket worker thread | `lib/obs-websocket-api.h:25,53-75,194-224` | test control |

## Discord

| Behaviour | Source | Used by |
|---|---|---|
| Five Opus silence frames `F8 FF FE` before a break in transmission; voice connections only, no spacing given | Discord API docs `developers/topics/voice-connections.mdx:502-504` | `IdleMode::Documented` |
| Go Live idling behaviour | undocumented | measured once by a manual test against real Discord |
| E2EE (DAVE) mandatory for voice and Go Live from 2026-03-01 | `voice-connections.mdx:227-236` | existing transport |
| Go Live limits used by the client: up to 1920×1080, 15, 30 or 60 fps | `rust/crates/dstreamy-core/src/stream_config.rs:49-55,117-150` (our own limits, inherited from the Bun daemon; Discord documents none) | producer validation |
| davey may reject the first keyframe after DAVE comes up, so the pump requests another 500 ms later | `rust/crates/dstreamy-discord/src/stream/pump.rs:20-21,473-481` | keyframe consequences |
| Reference client ignores PLI and FIR; asks for keyframes every 1 s and no B-frames | `@dank074/discord-video-stream` `src/client/voice/WebRtcWrapper.ts:175-176,207-209`, `src/media/newApi.ts:29,283-287` | keyframe policy |
| PLI/FIR become keyframe requests in our transport (our vendor patch `f5b44f0`, not upstream) | `rust/vendor/discord-stream-rs/src/voice/webrtc.rs:332-344` | `ExternalSource`, mock tests |
| Gateway close: the client sends Close and reads until the server's Close (at most 2 s), then may drop TCP; closing with unread data sends RST, which can discard what the peer has not read yet. Discord's own close behaviour is unverified; the mock follows the RFC | RFC 6455 §1.4 (RST), §5.5.1 ("MAY close the connection at any time after sending and receiving a Close message"); `tungstenite` `src/protocol/mod.rs:256-276`; lost op 4 on macOS CI, runs `40190910321` and `40193700980` | `close_handshake` in `rust/crates/dstreamy-discord/src/gateway/connection.rs`; the test pins ordering, not RST |

## H.264

| Behaviour | Source | Used by |
|---|---|---|
| Annex B start code `00 00 01`; a leading `zero_byte` before SPS, PPS and the first NAL of an access unit | H.264 §B.1.1, §B.1.2 | ingest validation, packet conversion |
| NAL types: 5 IDR slice, 6 SEI, 7 SPS, 8 PPS, 9 AUD; type is the low 5 bits of the header byte | H.264 §7.3.1, Table 7-1 | ingest validation |
| AUD first in an access unit; SEI before the primary coded picture | H.264 §7.4.1.2.3 | packet conversion |
| Emulation prevention byte `0x03` | H.264 §7.3.1, §7.4.1 | parser tests |

## Opus

| Behaviour | Source | Used by |
|---|---|---|
| TOC byte `config(5) s(1) c(2)`; `s` is per packet; codes 0–3 (1 frame, 2 equal, 2 sized, M frames). Table 2 frame sizes: configs 0–11 SILK NB/MB/WB 10/20/40/60 ms, 12–15 Hybrid SWB/FB 10/20 ms, 16–31 CELT NB/WB/SWB/FB 2.5/5/10/20 ms | RFC 6716 §3.1, Table 2 | ingest validation |
| Frame lengths: one byte 0–251, else `b0 + 4·b1` (at most 1275); code 3 adds a frame count byte `v p M`, optional padding lengths (255 = 254 plus another byte) and, for VBR, M−1 lengths | RFC 6716 §3.2.1, §3.2.5 | ingest validation |
| A receiver refuses packets that break R1 (at least one byte), R2 (no frame over 1275 bytes), R3 (code 1 payload even), R4 (code 2 length fits), R5 (code 3 has 1 to 120 ms of frames), R6 (code 3 CBR: padding fits and the rest divides by M), R7 (code 3 VBR: headers, M−1 frames and padding fit) | RFC 6716 §3.4 | ingest validation |
| `F8 FF FE` is config 31 (CELT FB 20 ms), mono, code 0 | RFC 6716 §3.1, §3.2.2 | idle frames, 20 ms spacing |
| Mono frames are valid in a stereo stream | RFC 6716 §2.1.2, RFC 7587 §3.4 | idle frames |
| RTP clock 48 kHz; one Opus packet per RTP payload; 20 ms = 960 ticks | RFC 7587 §4.1, §4.2 | timeline |

## RTCP feedback

| Behaviour | Source | Used by |
|---|---|---|
| PLI (PSFB FMT 1): a reaction is optional | RFC 4585 §6.3.1 | keyframe policy |
| FIR (PSFB FMT 4): the encoder MUST send a refresh point. libobs has no API for it; we deviate, bounded by `keyint_sec` ≤ 2 | RFC 5104 §4.3.1.2 | keyframe policy, ADR 0004 |

## Platform

| Behaviour | Source | Used by |
|---|---|---|
| `SO_PEERCRED` gives the peer's credentials in effect at connect; the uid is the effective uid (`cred->euid`), compared with `geteuid(2)` ("returns the effective user ID"). macOS `getpeereid` also gives the effective uid | `unix(7)` "SO_PEERCRED"; Linux v6.12 `net/core/sock.c` `cred_to_ucred`; `geteuid(2)`; XNU `f6217f89` `bsd/kern/kern_credential.c:1738`, `bsd/sys/ucred.h:160`, `bsd/kern/uipc_usrreq.c:1433` | peer check |
| Connecting to a Unix socket needs write permission on the socket file and search permission on its directory on Linux; POSIX is silent and some BSDs ignore socket permissions. macOS enforces write permission in `unp_connect` | `unix(7)` "Pathname socket ownership and permissions"; XNU `f6217f89` `bsd/kern/uipc_usrreq.c:1345` (`vnode_authorize(… KAUTH_VNODE_WRITE_DATA …)`) | private layout, cross-user test |
| A stale socket gives `ECONNREFUSED` on connect; binding over an existing path gives `EADDRINUSE` | `unix(7)` ERRORS; `connect(2)` ERRORS | stale-socket replacement |
| `sun_path` is limited to 108 bytes on Linux and 104 on macOS; macOS `$TMPDIR` is long (`/var/folders/…`) | `unix(7)`; XNU `f6217f89` `bsd/sys/un.h:79` | endpoint path: fall back to the home directory when too long |
| `getpeereid` gives the peer's effective uid and gid | macOS `getpeereid(3)`, `Libc gen/FreeBSD/getpeereid.c:41-46` | macOS peer check |
| `UnixStream::peer_cred`; `UCred::pid()` is `Option` | tokio `src/net/unix/stream.rs:980-983`, `src/net/unix/ucred.rs:7-45,292-336` | `Peer` |
| `File::try_lock` stable since 1.89.0; `flock` on Unix, `LockFileEx` over the whole file on Windows | `library/std/src/fs.rs:939-976`, `sys/fs/unix.rs:1369-1393`, `sys/fs/windows.rs:458-478` | session lock |
| `File::open` on Unix sets `O_CLOEXEC`, so a spawned child never inherits the lock fd and the lock dies with the process | `library/std/src/sys/fs/unix.rs:1233` at Rust 1.99.0 | session lock release on crash |
| An exclusive `LockFileEx` lock denies other processes reads | `LockFileEx` docs `:131-135` | separate `session.pid` |
| `CreateNamedPipeW` default DACL grants Everyone read; an explicit DACL is needed | `CreateNamedPipeW` docs `:373-376` | Windows transport |
| `PIPE_REJECT_REMOTE_CLIENTS`; `FILE_FLAG_FIRST_PIPE_INSTANCE` fails a second instance with `ERROR_ACCESS_DENIED` | `CreateNamedPipeW` docs `:133-139,340-345` | Windows transport |
| `GetNamedPipeClientProcessId` returns the client's process id; whether it works before any read is not stated. `ImpersonateNamedPipeClient` uses "the last message read" | `sdk-api-src/content/winbase/nf-winbase-getnamedpipeclientprocessid.md:62`, `content/namedpipeapi/nf-namedpipeapi-impersonatenamedpipeclient.md:80-90` | Windows peer check (provisional until tested) |
| Checking the peer's user: `GetNamedPipeClientProcessId` (server side) or `GetNamedPipeServerProcessId` (client side), then `OpenProcess`, `OpenProcessToken`, `GetTokenInformation(TokenUser)`; a pid can be reused after the peer exits | `sdk-api-src/content/winbase/nf-winbase-getnamedpipeclientprocessid.md`, `nf-winbase-getnamedpipeserverprocessid.md`, `content/processthreadsapi/nf-processthreadsapi-openprocess.md`, `nf-processthreadsapi-openprocesstoken.md`, `content/securitybaseapi/nf-securitybaseapi-gettokeninformation.md` at the pinned sdk-api commit | Windows peer check (provisional until the Windows transport is built and tested) |
| Windows console events: `CTRL_C_EVENT`, `CTRL_CLOSE_EVENT`, `CTRL_SHUTDOWN_EVENT`; tokio exposes `ctrl_c`, `ctrl_close`, `ctrl_shutdown` | `SetConsoleCtrlHandler` / `HandlerRoutine` docs; tokio `src/signal/windows.rs` at 1.54.0 | stopping `serve` on Windows |
| Restricting a file to its owner on Windows needs an explicit DACL (no mode bits) | `SetNamedSecurityInfoW` and security-descriptor docs at the pinned sdk-api commit | token and config files on Windows (provisional) |
| tokio `first_pipe_instance` duplicate → `PermissionDenied`; `reject_remote_clients` defaults to true; `create_with_security_attributes_raw` is `unsafe` and takes null or a valid `SECURITY_ATTRIBUTES` | tokio `src/net/windows/named_pipe.rs:1779,1995-2057,2163-2173,2283-2370` | Windows transport |

## Service managers

| Behaviour | Source | Used by |
|---|---|---|
| `Restart=on-failure` restarts after SIGKILL but not after SIGTERM, SIGINT, SIGHUP or SIGPIPE; `RestartSec` default 100 ms; start limit 5 per 10 s | `systemd.service(5)` `man/systemd.service.xml:573-577,792-900`, `systemd-system.conf.xml:424-426` | Linux user unit |
| `systemd-analyze --user verify` | `systemd-analyze(1)` `:668-760,962-967` | installer test |
| `loginctl enable-linger` | `loginctl(1)` `:186-198` | CI setup |
| `KeepAlive.SuccessfulExit=false`, `RunAtLoad` default false, `ThrottleInterval` default 10 s | `launchd.plist(5)`; `launchd-842.92.1 man/launchd.plist.5:175-237` | macOS agent |
| How launchd classifies SIGKILL under `SuccessfulExit` | undocumented | measured by the macOS install test |
| `launchctl bootstrap\|bootout gui/<uid> <plist>`; `user/<uid>` exists without a GUI login | `launchctl(1)` | macOS installer |
| `RestartOnFailure`: `Interval` is the retry window (minimum PT1M), not a delay; "fails" is undefined | Task Scheduler schema, `restartonfailure` and `interval` elements | Windows task |
| Whether a killed process counts as a failure | undocumented | measured by a Windows CI test |
| `LogonType=InteractiveToken`; `MultipleInstancesPolicy` default `IgnoreNew` | Task Scheduler schema | Windows task |
| Default `ExecutionTimeLimit` stops a task after 72 h; `DisallowStartIfOnBatteries` defaults to true. For `StopIfGoingOnBatteries` the source contradicts itself (the XSD says `default="true"`, the Remarks say false) | `ITaskSettings::get_ExecutionTimeLimit` docs `:55`; schema elements | Windows task (sets `PT0S`, `false`, `false` explicitly) |
