# 12 — capture linux

Status: done
Depends: 03

## Scope

`dstreamy-capture-linux`: ScreenCast portal via ashpd (window/monitor, persist restore_token in config), PipeWire fd → GStreamer pipeline `pipewiresrc ! videoconvert ! videocrop ! <vaapih264enc|nvh264enc|x264enc> ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink` (H265 analogous), encoder auto-select with fallback. Audio: PipeWire capture of system/app node → PCM → Opus.

## Gates

- Container test: same pipeline with `videotestsrc` instead of pipewiresrc yields decodable AUs via x264enc
- Encoder selection logic unit-tested
- Portal + real capture `#[ignore]`, listed in MANUAL-CHECKLIST.md
- Global gates from PRD

## Comments

Evidence (2026-10-21):
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-12 rust/ci/linux.sh` → fmt, clippy `-D warnings` clean; `dstreamy-capture-linux` 17 passed, 1 ignored (portal); workspace green.
- macOS `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → green; the crate compiles to nothing there.
- Container tests: videotestsrc → same builder → x264enc → h264parse → appsink yields 60 AUs, first has SPS/PPS/IDR, all decode with openh264 at 1280x720, one keyframe per fps*2; aspect fit 1000x1000→720x720, no upscale 640x360; crop 321x241@(100,50) → 320x240; audiotestsrc → contiguous 20 ms Opus; fallback past a missing and an unnegotiable encoder; force-key-unit gives an IDR within 20 frames; stop closes the channel.
- Reviewer subagent pass: fixed runtime bus errors being silent (`last_error()`), video pts without timestamp, audio pts drift (rebase on >40 ms gap), fps overflow, blocking teardown in `Drop`, wrong `target-object` use.

Decision: Crop runs before scaling (`videoconvert ! videocrop ! videoscale`), unlike the order in the brief, because `CropRect` is in source pixels; videocrop uses left/top with right/bottom = -1 so the far edges come from the crop caps. Crop and output sizes round down to even.
Decision: Aspect-preserving fit uses caps `width=[2,W,2], height=[2,H,2], pixel-aspect-ratio=1/1` and lets videoscale fixate, which keeps DAR and never upscales, matching `CaptureDimensions.swift`; a crop is then also scaled down if it exceeds the configured size.
Decision: `videorate` + `pipewiresrc keepalive-time=1000/fps` give a constant frame rate on static screens.
Decision: Encoder fallback happens at start: each candidate is built and run until the first AU or a bus error (3 s probe); a missing element or a negotiation failure moves to the next. A failure after the first frame ends the stream (channel closes, `last_error()` says why) rather than switching encoder mid-stream.
Decision: Hardware encoders are fed NV12, x264/x265 I420; H.264 output caps request constrained-baseline/baseline, H.265 Main. Properties a given element version lacks or rejects are skipped instead of panicking (needs glib `v2_74` for `ParamSpec::value_is_valid`).
Decision: Audio prefers `pulsesrc device=@DEFAULT_MONITOR@` (pipewire-pulse) over `pipewiresrc stream.capture.sink=true`; missing both is `Unsupported` when audio is on.
Decision: `list_targets` returns one target, id `portal`, kind Display (Window if the source is set to windows only). The source kind is `PortalSources` on the builder; `with_restore_token`/`restore_token()` let the caller persist the token. A new token is kept even if the pipeline then fails, since the portal invalidates the old one.
Decision: The PipeWire fd stays owned by `PortalStream` (pipewiresrc dups it), so fallback can rebuild pipelines on the same fd.
Decision: No changes to `dstreamy-media` or CI packages were needed; the portal test also needs `gstreamer1.0-pipewire`/`-pulseaudio` on a desktop (MANUAL-CHECKLIST).
