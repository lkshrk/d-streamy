# 0002 — Vendor webrtc-rs so RTP senders use Discord-assigned SSRCs

Status: accepted (2026-10-21)

## Context

Discord's voice READY assigns the audio, video and RTX SSRCs, and the client announces them back in SPEAKING (op 5) and VIDEO (op 12). The JS reference (`@dank074/discord-video-stream` on libdatachannel) sends RTP with exactly those SSRCs, and its NACK responder and sender-report generator work on them.

`webrtc-rs` 0.14, which `discord-stream-rs` builds on, always picks a random SSRC per RTP sender and has no API to choose one. Writing packets with a different SSRC through the track would reach Discord, but SRTCP demultiplexes incoming RTCP by SSRC: NACKs and PLIs for the Discord SSRC land on an unread stream and are dropped, and sender reports go out under the random SSRC. Loss recovery and A/V sync would be lost.

## Decision

Vendor `webrtc` 0.14.0 under `rust/vendor/webrtc` and use it through `[patch.crates-io]`. The only patch adds a defaulted `TrackLocal::preferred_ssrc()` method that `RTCRtpSender` uses instead of a random SSRC when it returns `Some`. The Discord transport supplies the READY SSRCs through it.

## Consequences

- RTP, NACK retransmission and RTCP sender reports all use the Discord SSRCs, as in the reference.
- About 27k lines of third-party code are vendored. The crate is excluded from the workspace, so it is not linted or tested here, and upgrading means re-importing and re-applying one small patch.

## Rejected

- Announcing the random webrtc-rs SSRCs to Discord in op 5/op 12: unverified against Discord, and it departs from the production reference.
- Rewriting SSRCs in an interceptor: incoming RTCP for an unknown SSRC never reaches the interceptor chain.
- Bypassing `RTCRtpSender` and writing to the SRTP session directly: the session accessors are crate-private.
