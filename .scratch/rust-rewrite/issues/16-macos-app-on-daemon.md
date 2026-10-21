# 16 — macOS menu bar app on `dstreamy daemon`

Status: done
Depends: 15 (the protocol is fixed in ADR 0003, so work can start in parallel against the spec and fixtures)

## Scope

Rework the SwiftUI app (`App/`, `AppTests/`, `Package.swift`) to be a pure frontend:
- Replace `DaemonController`/`IPCMessages` with a client for protocol v1 (ADR 0003) that spawns `dstreamy daemon`. The bundled binary is `Contents/MacOS/dstreamy`. The dev fallback is `rust/target/debug/dstreamy`, or `DSTREAMY_BIN`.
- Remove every use of CaptureLib and of in-process capture: CaptureSession, WindowCapture, the pipe protocol. The window/display picker uses the daemon's `targets`. The crop overlay sends the crop in source pixels. The stats and health views read `SessionEvent` health snapshots. The token goes through `set_token`; drop any token from the old connect payload.
- `Package.swift`: the app target no longer depends on CaptureLib. Leave the `capture/` sources in place (issue 14 deletes them), but they must no longer build as part of the app.
- `release.yml` (Swift release): stop bundling bun and the JS daemon. Build `dstreamy` for aarch64 and x86_64 apple-darwin with the pinned toolchain, lipo them, copy the result into the app and codesign it inside the bundle. Keep the existing signing and notarisation flow and the `v*` tag trigger. Don't create tags.
- Update the README macOS section.

## Gates

- `swift build` and `swift test` pass locally and in the CI `build-test` job.
- The Swift tests decode every golden fixture from `rust/crates/dstreamy-cli/tests/fixtures/daemon/` (the app's own model types, no duplicated hand-written JSON).
- A Swift integration test (or a script run in CI on macOS) builds `dstreamy`, then drives the app's daemon client against `dstreamy daemon --gateway-url <mock>` (the mock started via a small Rust test helper binary or a test), covering hello → guilds → channels → start (test-pattern) → stop.
- actionlint on release.yml.
- Global gates from the PRD.
- Manual (MANUAL-CHECKLIST): launch the built .app, check that Screen Recording is attributed to D-Streamy, and stream a window to the burner account.

## Comments
- Decision: `Package.swift` drops the `CaptureLib` and `CaptureLibTests` targets entirely; the `capture/` sources stay on disk for issue 14. Keeping them as a separate target would keep building and testing code nothing uses.
- Decision: the app keeps one long-lived `dstreamy daemon` (spawned at launch, respawned on the next action if it exits) instead of one per stream. Concurrent callers share a single launch.
- Decision: the protocol has no logout or username, so "Logout" became "Change token" and the header shows "Logged in". A token in the old Keychain item (`dstreamy`/`discord-token`) is sent once with `set_token` when the daemon reports `logged_in: false`, then deleted from the Keychain. The app no longer calls the Discord REST API itself.
- Decision: the source picker is a menu over `targets` (displays and windows, with Refresh) and is disabled while a stream is live, because `update` cannot change the target. SCContentSharingPicker is gone. The selection is restored by target id, else by the same app and window title.
- Decision: the crop stays window-only. The overlay rect (points, bottom-left origin) is converted to source pixels using the target's pixel size from a fresh `targets` call, both at `start` and for each live `update` (debounced 300 ms). A crop drawn while connecting is sent when `streaming` arrives.
- Decision: Stop or Cancel while `start` is pending marks the start cancelled. Its state events are ignored, and `stop` is sent once `start` returns.
- Decision: uptime is measured in the app from the first `streaming` state; everything else in the stats and health views comes from the `health` events.
- Decision: `codec` is sent lowercase (`h264`/`h265`) and `resolution` as `WIDTHxHEIGHT`.
- Decision: `scripts/build.sh` bundles `DSTREAMY_DAEMON` when it is set, else builds `rust/target/release/dstreamy` for the host. `UNIVERSAL=1` builds the Swift app for arm64 and x86_64. `release.yml` runs on `macos-26` (screencapturekit needs the Xcode 26 SDK), builds both Rust targets with the pinned toolchain, lipos them, and runs `release.sh` with `DSTREAMY_DAEMON` and `UNIVERSAL=1`. `release.sh` signs `Contents/MacOS/dstreamy` hardened and timestamped before the app; bun, its JIT entitlements and the Resources Mach-O loop are gone. I also fixed the existing shellcheck findings (quoting) so actionlint passes.
- Decision: the CI `build-test` job moves to `macos-26`, builds `dstreamy-cli` and `dstreamy-mock-discord`, and runs `swift test` with `DSTREAMY_INTEGRATION=1`, so the integration test fails instead of skipping when the binaries are missing. The Bun steps stay until issue 14.
- Evidence (2026-10-21, macOS 27, Xcode 27.1, Swift 6.4):
  - `swift build`: OK, no warnings.
  - `cd rust && cargo build --locked -p dstreamy-cli -p dstreamy-mock-discord && cd .. && DSTREAMY_INTEGRATION=1 swift test`: 13 tests pass (3 runs, plus 6 runs of the client and integration tests). `DaemonFixtureTests` decodes every line of `rust/crates/dstreamy-cli/tests/fixtures/daemon/*.ndjson` with the app's own types: requests re-encode to the same JSON, replies decode into the type of the matching command, and the events cover state and health. `DaemonIntegrationTests` drives `DaemonClient` against `dstreamy --config-dir <tmp> --gateway-url <mock> daemon --skip-platform` and the `mock-discord` launcher: hello → logged_in → set_token → guilds → channels → targets → start(test-pattern) → streaming plus a health event with frames sent → update(crop) → stop (frames sent > 0) → stopped(requested) → shutdown → exit 0.
  - `actionlint .github/workflows/*.yml`: clean. `shellcheck scripts/build.sh scripts/release.sh`: clean.
  - `SKIP_SIGN=1 DSTREAMY_DAEMON=rust/target/debug/dstreamy bash scripts/build.sh`: the bundle has `Contents/MacOS/{D-Streamy,dstreamy}`, and `dstreamy --version` prints 0.1.0. `swift build -c release --arch arm64 --arch x86_64` produces a fat x86_64+arm64 `D-Streamy`.
  - Rust gates (`cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`): exit 0. No Rust files changed.
  - Reviewer subagent: 17 findings. Fixed: the double daemon spawn, cancel during connecting, stdout being read to EOF before pending requests fail, health-event ordering, the stale target size for the crop, process state set before `run()`, the Keychain call off the main actor, crop changes during connecting, deinit failing pending requests, Int64 in JSONValue, two flaky test assumptions, the universal app build, the release.sh warning, the added banner comments, and setting the app delegate at launch. The live-crop `update` was a daemon-side gap, closed by issue 15 (91f1529).
  - Not tested here: the GUI itself and Screen Recording attribution (MANUAL-CHECKLIST), the signed and notarised release run (needs a `v*` tag), and the CI run on `macos-26` (not pushed).

- Commit: d3eb493.
