# 0006 — GStreamer element selection on Linux

Status: accepted (2026-11-06). Details 0001 (Linux capture: PipeWire + GStreamer).

## Context

ADR 0001 names three encoder families for Linux (VA-API, NVENC, x264) and leaves the choice of elements open. Which element runs depends on what the user installed and on the GPU driver, and a candidate that is installed can still fail to negotiate. Audio capture has a trap of its own: PipeWire links a capture stream whose chosen device is missing to the default source, which is usually the microphone.

## Decision

- **Video encoders are tried in a fixed order, best first, limited to installed factories:** `vah264enc`, `vaapih264enc`, `nvh264enc`, `x264enc` for H.264 and `vah265enc`, `vaapih265enc`, `nvh265enc`, `x265enc` for H.265 (`rust/crates/dstreamy-capture-linux/src/encoder.rs`, `H264_ORDER`). The GPU's own VA plugin comes first, the older `vaapi` elements second, NVENC third, software last. A candidate whose pipeline fails to negotiate is dropped and the next one is tried (`pipeline.rs`); the stream fails only when every candidate failed. Each element gets the low-latency CBR properties and the raw input format it needs from `encoder_spec`.
- **System audio comes from `pulsesrc`** (through pipewire-pulse) when installed, else from `pipewiresrc`; without a chosen device it captures the default sink's monitor.
- **A chosen device (`--audio-source`, `audio_source` in `config.toml`) is pinned.** The node gets `node.dont-fallback`, `node.dont-reconnect` and `node.dont-move`, so a missing or removed device ends the capture with an error instead of silently switching to the default source. Aliases such as `@DEFAULT_SOURCE@` are refused, because they can never match the pinned device. A source name that is or starts with a number is refused on `pulsesrc`, where pipewire-pulse would read it as a device index.
- **Audio sources never provide the pipeline clock** (`provide-clock=false`), so the audio branch can be replaced while the video branch keeps running.

## Consequences

- A machine without a hardware encoder still streams, through `x264enc`.
- A user who unplugs the chosen audio device loses the stream's audio with an error, instead of streaming the microphone.
- The order is a policy, not a measurement: a slow or broken hardware element that negotiates is used.

## Rejected

- Trusting a named device as a preference, as GStreamer does by default: the fallback streams the microphone without consent.
- Runtime plugins or a user-chosen element name: no stable ABI, and the fixed order already covers the elements that exist.
