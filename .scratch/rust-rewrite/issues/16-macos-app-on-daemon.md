# 16 — macOS menu bar app on `dstreamy daemon`

Status: ready-for-agent
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
