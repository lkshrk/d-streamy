# Protocol audit: discord-stream-rs 0.1.3 vs @dank074/discord-video-stream 6.0.0

Reference: `node_modules/@dank074/discord-video-stream/src` (v6.0.0, in production in dstreamy today, on `@lng2004/node-datachannel` 0.32 and `@snazzah/davey` 0.1.11).
Audited: `rust/vendor/discord-stream-rs` as imported from crates.io 0.1.3 (upstream commit `50a402c`), then patched.
Status column: **patched** (commit on this branch), **same** (already equivalent), **justified** (deliberate difference, reason given).

Headline: upstream 0.1.3 was a partial port. The DAVE/MLS opcode handling and the SPS rewriter were close to the reference, but the Streamer signalling was stubbed out, the voice connection never started or heartbeated, no SELECT_PROTOCOL or remote SDP existed, and the WebRTC sender put each whole frame into one RTP packet on a random SSRC with H264's payload type for every codec. None of it could have streamed. All of that is patched.

## 1. Main gateway (client → Discord, via the caller's gateway WS)

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| Join: op 4 VOICE_STATE_UPDATE | `{guild_id, channel_id, self_mute:false, self_deaf:true, self_video:false}` | same | same |
| Leave: op 4 | `{guild_id:null, channel_id:null, self_mute:true, self_deaf:false, self_video:false}` | same | same |
| Go Live: op 18 STREAM_CREATE + op 22 STREAM_SET_PAUSED | `{type, guild_id, channel_id, preferred_region:null}` then `{stream_key, paused:false}` | `signal_stream()` empty | patched `a01cea0` |
| Stop: op 19 STREAM_DELETE | `{stream_key}` | `signal_stop_stream()` empty | patched `a01cea0` |
| op 20 STREAM_WATCH / 21 STREAM_PING | unused | unused | same |
| Opcode enum | 0–39 | 0–30 | patched `1820e60` |
| Stream key | `guild:<g>:<c>:<u>` / `call:<c>:<u>` | same | same |

Dispatches consumed:

| Event | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| VOICE_STATE_UPDATE | own `user_id` only → `setSession(session_id)` | stored, never started a connection | patched `a01cea0` |
| VOICE_SERVER_UPDATE | ignore other `guild_id`, ignore other `channel_id` if present → `setTokens(endpoint, token)` | no filtering; start was a placeholder | patched `a01cea0` |
| STREAM_CREATE | key must match guild/channel/user → stream conn with `serverId = rtc_server_id`, voice session id | needed a stream connection nobody created | patched `a01cea0` |
| STREAM_SERVER_UPDATE | key must match → `setTokens` | no key check | patched `a01cea0` |
| `endpoint: null` | passed through (would connect to `wss://null`) | `String`, failed to parse | justified: `Option`, null is ignored |
| Channel move (vault pitfall `discord-channel-move`) | dstreamy patches the private `channelId` | n/a | `Streamer::set_voice_channel()` retargets the next `create_stream()` without rejoining |

## 2. Voice gateway

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| URL / version | `wss://{endpoint}/?v=8` | same | same; a `ws://`/`wss://` endpoint is used as is (test servers) |
| op 0 IDENTIFY | `server_id, user_id, session_id, token, video:true, streams:[{type:"screen",rid:"100",quality:100}], max_dave_protocol_version` | no `max_dave_protocol_version` | patched `7801232` |
| IDENTIFY `server_id` for streams | `rtc_server_id` | guild id | patched `a01cea0` |
| op 8 HELLO → op 3 HEARTBEAT | every `heartbeat_interval` ms, `{t: now_ms, seq_ack}` | timer never armed (0.75× computed, unused) | patched `a01cea0` |
| HEARTBEAT_ACK (op 6) | ignored | ignored | justified: tracked; 3 unacknowledged heartbeats close the session and resume (`a33107b`), so a half-open WS is detected
| `seq` tracking | JSON `seq`, binary bytes 0–1 | same | same |
| op 2 READY | audio ssrc, `streams[0].ssrc/rtx_ssrc`, ip, port, modes | parsed only | patched `a01cea0`: drives WebRTC init |
| op 1 SELECT_PROTOCOL | `{protocol:"webrtc", codecs:Object.values(CodecPayloadType), data:sdp, sdp, rtc_connection_id:uuid}` | never sent | patched `a01cea0` (same keys, incl. `clockRate`, `encode`, `decode`) |
| op 12 VIDEO after READY | video off: `{audio_ssrc, video_ssrc:0, rtx_ssrc:0, streams:[]}` | missing | patched `a01cea0` |
| op 12 VIDEO (`setVideoAttributes`) | stream `rid:"100"`, `quality:100`, `max_bitrate:10_000_000`, `max_framerate`, `max_resolution{type:"fixed"}` | missing | patched `a01cea0` |
| op 5 SPEAKING | `{delay:0, speaking:1 (voice) / 2 (stream) / 0, ssrc:audio}` | missing | patched `a01cea0` |
| op 4 SELECT_PROTOCOL_ACK | `initDave()` then build answer SDP, then resolve "ready" | DAVE only | patched `a01cea0` |
| Answer SDP rejected locally | `setRemoteDescription` error unobserved | n/a | justified: `ready()` resolves with the error (`a33107b`) |
| op 7 RESUME / op 9 RESUMED | on close code 4015 or < 4000, reconnect with `{server_id, session_id, token, seq_ack}` | no reconnect | patched `a01cea0` |
| Reconnect pacing | immediate, unbounded | n/a | justified: 1 s delay, give up after 5 failed connects or sessions that end before READY/RESUMED (`MAX_CONNECT_ATTEMPTS`, `a33107b`), then `VoiceEvent::Closed`; the session supervisor (issue 08) owns higher-level retries |
| Messages while the WS is reconnecting | dropped (`readyState !== OPEN`) | n/a | justified: queued and sent after RESUME |
| ops 11/13 CLIENTS_CONNECT / CLIENT_DISCONNECT | track user ids for MLS | same | same |
| ops 14–20 | ignored | ignored | same |
| `op >= 4000` branch | logs | unreachable (`u8`) | justified: removed; close codes are handled on WS close |

## 3. DAVE (E2EE, `davey`)

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| Library | `@snazzah/davey` 0.1.11 (NAPI over the Rust crate) | `davey` 0.1.5 | same protocol version (`DAVE_PROTOCOL_VERSION = 1`) |
| DAVE channel id | voice: channel id; stream: `rtc_server_id - 1` | voice channel id for both | patched `a01cea0` |
| `initDave()` | v>0: new/reinit session, send op 26 key package; v=0: reset + passthrough(true, 10) | same | same |
| Client binary frame | `[op][payload]` | `[seq:u16][op][payload]` | patched `b36d26a` |
| bin 25 MLS_EXTERNAL_SENDER | `setExternalSender` | same | same |
| bin 27 MLS_PROPOSALS | `processProposals(optype, payload, connectedUsers)` → bin 28 commit‖welcome | same | same; justified: an empty user set is passed as "no expectation" (JS passes `[]`) |
| bin 29 ANNOUNCE_COMMIT / bin 30 WELCOME | process; if `transition_id>0` store pending + op 23 TRANSITION_READY; on error op 31 + re-init | same | same |
| op 21 PREPARE_TRANSITION | store; id 0 → execute now; else v=0 → passthrough(true, 120), op 23 | same | same |
| op 22 EXECUTE_TRANSITION | set protocol version; downgrade flag; upgrade from downgrade → passthrough(true, 10) | version never changed; downgrade cut passthrough to 10 s | patched `315dd8e` |
| op 24 PREPARE_EPOCH | epoch 1 → set version, `initDave()` | same | same |
| Encrypt gate (`daveReady`) | `protocolVersion != 0 && session.ready` | `session.ready` only | patched `315dd8e` |
| Media encryption | Opus: `encryptOpus`; video: `encrypt(VIDEO, codec, frame)` on the whole Annex B frame before packetization | same | same |

DAVE and packetization interact: davey rewrites start codes to 4 bytes and records unencrypted ranges per NAL unit, so the packetizer must not drop, merge or reorder NAL units (webrtc-rs's own `H264Payloader` drops AUD/filler and aggregates SPS+PPS). The new packetizer keeps every NAL unit, like libdatachannel.

## 4. Transport encryption

| Item | JS reference | Rust | Status |
|---|---|---|---|
| READY `modes` (`aead_aes256_gcm_rtpsize`, `aead_xchacha20_poly1305_rtpsize`) | stored, unused (UDP-only) | stored, unused | same |
| Media transport | DTLS-SRTP (libdatachannel) | DTLS-SRTP (webrtc-rs) | same; SRTP profile negotiated in DTLS (webrtc-rs offers AEAD_AES_128_GCM, AEAD_AES_256_GCM, AES_CM_128_HMAC_SHA1_80 and _32) |
| DTLS role | answer `a=setup:passive` → client is DTLS client | same | same |

## 5. WebRTC / SDP

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| Peer connection | 1 audio (mid 0) + 1 video (mid 1) track, ICE `stun:stun.l.google.com:19302` | same, but never driven | patched `684e30b`; ICE servers and webrtc-rs `SettingEngine` configurable |
| Offer | sent as-is in SELECT_PROTOCOL | not created | patched `684e30b`/`a01cea0` |
| Offer contents | libdatachannel: no `a=ssrc`, RTX rtpmaps, H264 fmtp `profile-level-id=42e01f;packetization-mode=1` | webrtc-rs: `a=ssrc` with the Discord SSRCs, no RTX, empty H264 fmtp | justified: Discord reads codecs from the SELECT_PROTOCOL `codecs` JSON, which matches the reference exactly. Listed for the manual live check |
| Remote answer | synthesised from Discord's SDP lines (`c=`, `a=rtcp` port, ice-ufrag/pwd, fingerprint, last candidate); extmaps and rtpmaps as in `handleProtocolAck()` | none | patched `684e30b` |
| Answer direction | `a=inactive` | n/a | justified: `a=recvonly`; webrtc-rs honours direction and would not send on inactive transceivers (libdatachannel ignores it) |
| Answer session lines | none | n/a | justified: `v=/o=/s=/t=`, `a=group:BUNDLE 0 1`, `a=ice-lite` added because the webrtc-rs parser requires them |
| Peer connection loss | re-create on `closed` (not by us) and send SELECT_PROTOCOL again | n/a | justified: re-create on `Failed`; webrtc-rs never reaches `Closed` by itself |
| `ready` | `pc.state() === "connected"` | same | same |

## 6. RTP

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| SSRCs | audio = READY `ssrc`, video = `streams[0].ssrc` | random webrtc-rs sender SSRCs (header SSRC overwritten) | patched `299e81a` (vendored webrtc, ADR 0002) + `684e30b` |
| Payload types | opus 120; H264 101/rtx 102, H265 103/104, VP8 105/106, VP9 107/108, AV1 109/110 | same table; video track always rewrote to 101 | patched `684e30b` |
| Clock rates | 48 kHz / 90 kHz | same | same |
| Timestamp | random start; after each send `+= round(frametime * clock / 1000)` | same formula | same; plus `send_*_frame_at(pts)`: `start + pts * clock` |
| Sequence | random start, +1 per packet | same | same |
| Header extensions | playout-delay (id 5) on every packet: audio min 0 / max 1, video 0 / 10 (×10 ms) | none | patched `684e30b` |
| Other extmaps (audio-level, transport-cc, abs-send-time, toffset, video-orientation) | negotiated in the answer, not written by the packetizers | n/a | same |
| H264 packetization | libdatachannel `H264RtpPacketizer(StartSequence)`: single NAL or FU-A | whole frame in one packet | patched `684e30b`: single NAL ≤ 1200 B, else FU-A; nothing dropped or aggregated |
| H265 packetization | `H265RtpPacketizer(StartSequence)`: single NAL or FU (type 49) | same bug | patched `684e30b` |
| AV1 | `AV1RtpPacketizer(Obu)` | n/a | justified: rejected (`UnsupportedCodec`, `cdee276`); dstreamy streams H264/H265 only |
| VP8/VP9 | throws "Packetizer not implemented" | n/a | same (rejected) |
| Fragment size | libdatachannel default max fragment (≈1200 B) | n/a | 1200 B payload (`MAX_PAYLOAD_SIZE`) |
| Marker bit | last packet of each frame | set on every packet | patched `684e30b` (Opus: every packet, one per frame) |
| Opus | one packet per frame | same | same |

## 7. RTCP, pacing, retransmission

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| Sender reports | `RtcpSrReporter` on audio and video | default interceptors bound to the random SSRC | patched `684e30b`: webrtc-rs SR interceptor on the Discord SSRCs; RTCP is read so it runs |
| NACK | `RtcpNackResponder` resends from history on the same SSRC | NACKs for the Discord SSRC were dropped by SRTCP demux | patched `299e81a`/`684e30b`: webrtc-rs `Responder`, `nack` feedback on audio and video |
| RTX | RTX codecs offered, not used for resends | n/a | same behaviour (no RTX stream); justified |
| Pacing | `PacingHandler(25 Mbit/s, 1 ms)` on video | none | patched `684e30b`: token bucket at 25 Mbit/s, 1 ms granularity, video only; queue bounded at 8192 packets, a frame that does not fit is dropped whole (`a33107b`) |
| PLI/FIR | ignored | ignored | same; PLI is logged at debug |
| TWCC | negotiated, no sender | n/a | same |

## 8. H264 SPS VUI rewrite

| Item | JS reference | Rust 0.1.3 | Status |
|---|---|---|---|
| Algorithm | port of WebRTC `sps_vui_rewriter.cc`: force VUI + bitstream_restriction, `max_num_reorder_frames=0`, `max_dec_frame_buffering=max_num_ref_frames`, strip video_signal_type | same | same; output pinned byte-for-byte against `rewriteSPSVUI()` |
| When | only frames that contain an SPS; rebuilt with 3-byte start codes | every frame rebuilt with 4-byte start codes | patched `684e30b` |
| Malformed SPS | throws (dstreamy drops the frame) | `assert!` panic, possible infinite loop / overflow | patched `2ae9eeb`: SPS passed through unchanged |
| `se(v) = 0` | `writeSignedExpGolomb(0)` throws (JS bug) | correct | justified (Rust is right) |
| Scaling lists | reads `delta_scale` for every entry even after `nextScale == 0` (spec 7.3.2.1.1.1 stops), corrupting SPS with terminated lists | same bug | patched `fa94b91` (diverges from JS; only affects High-profile SPS with scaling matrices) |
| Exp-Golomb writes near i32/u32 limits | n/a | overflow panic | patched `fa94b91` |

## 9. Not ported / out of scope

- `BaseMediaStream`/`newApi` (FFmpeg demux, pts-synchronised sending): dstreamy calls `sendVideoFrame`/`sendAudioFrame` directly; the Rust `media` module stays behind the off-by-default `media` feature.
- `setStreamPreview` (HTTP to discord.com): dstreamy runs with preview off.
- The main gateway client itself (issue 06).

## Public send API

```rust
// discord_stream_rs::MediaConnection (re-exported as dstreamy_discord::transport::MediaConnection)
pub async fn set_packetizer(&self, video_codec: &str) -> Result<(), ConnectionError>;
pub async fn send_video_frame(&self, frame: &[u8], frametime_ms: f64) -> Result<(), WebRtcError>; // JS sendVideoFrame
pub async fn send_audio_frame(&self, frame: &[u8], frametime_ms: f64) -> Result<(), WebRtcError>; // JS sendAudioFrame
pub async fn send_video_frame_at(&self, frame: &[u8], pts: Duration) -> Result<(), WebRtcError>;
pub async fn send_audio_frame_at(&self, frame: &[u8], pts: Duration) -> Result<(), WebRtcError>;
pub fn set_video_attributes(&self, attrs: Option<VideoAttributes>) -> Result<(), ConnectionError>;
pub fn set_speaking(&self, speaking: bool) -> Result<(), ConnectionError>;
```

`frame` is one Annex B access unit (video) or one Opus packet (audio). Sends return `WebRtcError::NotConnected` (and do not advance the clock) until the peer connection is connected, like the JS early return.

## Verification

- Unit tests: packetizer (bit-exact FU-A/FU reassembly, markers, playout-delay), SPS rewrite vs JS output, binary framing, DAVE transitions, SELECT_PROTOCOL codecs, op 12 payloads, answer SDP parse.
- `tests/webrtc_loopback.rs`: real DTLS-SRTP to a webrtc-rs peer; RTP arrives on the READY SSRCs with PT 101/120 and the H264 frame (incl. AUD) reassembles bit-identically.
- `tests/streamer_flow.rs`: fake voice gateway + webrtc-rs peer drive join → IDENTIFY → READY → SELECT_PROTOCOL → ACK → heartbeat → speaking/video → RTP → close 4015 → RESUME → RESUMED → STREAM_CREATE/SET_PAUSED → stream IDENTIFY with `rtc_server_id` → STREAM_DELETE → leave; and channel-move retargeting.
- `crates/dstreamy-mock-discord` (issue 05): an independent server-side implementation (it does not link `discord-stream-rs`) of the main gateway, voice gateway, WebRTC answerer and DAVE. Its self-test streams `TestPatternSource` H264 + Opus through `Streamer` and the real `DiscordClient` over zlib-stream: the client's DAVE session forms an MLS group with a simulated participant (bin 25 → 26 → 27 → 28 → 29, op 23 → 22) and every frame arrives DAVE-encrypted and decrypts bit-identically (H264 NAL units after the SPS VUI rewrite, H265 incl. FU, Opus). No new vendor bugs were found. The vendored tests' WebRTC fakes now come from this crate.
- Mock caveat: the client offers H264 without fmtp, which webrtc-rs only matches partially, so an answerer that also accepts an exactly matching codec (H265) drops H264. The mock's media server accepts Opus and the one video codec it announces in SELECT_PROTOCOL_ACK. Real Discord picks the codec from the SELECT_PROTOCOL `codecs` JSON, so this does not affect production.
- Not verifiable here: real Discord acceptance of the webrtc-rs offer SDP and DAVE against Discord's MLS server (manual checklist).
