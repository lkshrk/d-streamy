# 0010 — How shipped binaries get their H.264 encoder

Status: accepted (2026-11-07). Release builds ship no H.264 software encoder.

## Context

`dstreamy-media` depends on `openh264 = "0.9"` with its default `source` feature. `OpenH264Encoder::new` in `rust/crates/dstreamy-media/src/h264.rs` uses `OpenH264API::from_source()`, so CLI binaries currently include a source-built codec and their notices say so.

Only the diagnostic `TestPatternSource` uses that encoder in normal application paths. Native Linux capture uses GStreamer and native macOS capture uses VideoToolbox. External-source idle sends fixed Opus silence packets. The OBS plugin uses OBS's encoders and its own workspace does not depend on OpenH264. None of those paths requires this encoder to run. Test producers and decoder-based tests also use source-built OpenH264.

Cisco's [official FAQ](https://www.openh264.org/faq.html) says its royalty coverage applies to its own distributed binary module, not a third party's source build, and does not guarantee coverage of other patent claims. The [binary license](https://www.openh264.org/BINARY_LICENSE.txt) requires a separate download to the end user's device, user control over enabling the binary, the Cisco notice where that control lives, and reproduction of the license text. It also warns that content providers and broadcasters may need a separate use license. These are licensing conditions, not a claim of distributor immunity or legal advice. The source's BSD license and patent licensing are separate questions.

## Options

1. **Runtime loading of Cisco's binary.** The crate's `libloading` feature provides `OpenH264API::from_blob_path(path)` with known-build hash checks. A design would need pinned platform downloads, a cache, enable/disable controls, the exact notice "OpenH264 Video Codec provided by Cisco Systems, Inc." and full license text, and actionable offline or disabled-codec errors. It should limit codec availability failures to the diagnostic test pattern, not prevent native capture, external idle or OBS streaming. Hash verification alone does not establish licensing compliance.
2. **Retain the source build.** This is the current code, not an accepted choice to assume or waive patent obligations. BSD notices do not extend Cisco's binary royalty coverage to it.

## Decision

Neither option. Release builds include no OpenH264 at all.

- `dstreamy-media` builds `OpenH264Encoder` only with its non-default `openh264` feature. `dstreamy-cli` enables that feature through `test-pattern`, and `test-hooks` turns on `test-pattern`.
- Without the feature, `TestPatternSource` lists no target. Starting `test-pattern` fails with an error that says to stream a display, window or portal target instead. Native capture, external idle and the OBS plugin are not affected.
- Tests, the macOS CI daemon (`--features test-pattern`) and the test producers still use source-built OpenH264. These are test builds that are never distributed.
- The release workflows and `rust-ci.yml` reject a normal build graph that enables `openh264` or `test-pattern`. `scripts/third-party-notices.py` lists OpenH264 only when `openh264-sys2` is in the shipped graph, so release notices no longer list it.

Option 1, a runtime loader for Cisco's binary, can be added later if a shipped feature needs H.264 software encoding. It would need its own ADR.

## Consequences

- Distributed binaries contain no H.264 encoder, so neither Cisco's binary terms nor the patent question for the source build applies to them.
- `dstreamy stream --target test-pattern` works only in test builds (`cargo build -p dstreamy-cli --features test-pattern`).
- Adding a shipped feature that needs software H.264 must reopen this ADR rather than turning on the feature in a normal dependency.
