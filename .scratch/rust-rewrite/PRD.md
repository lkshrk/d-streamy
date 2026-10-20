# PRD — D-Streamy Rust rewrite

See `docs/adr/0001-rust-rewrite.md` for the decision. This PRD is written for **unattended agent execution**: every issue ends in machine-checkable gates, and human-only steps are collected in `MANUAL-CHECKLIST.md`.

## Goal

`dstreamy` is a single Rust binary for macOS and Linux that streams a window or display (optionally cropped), with audio, to a Discord voice channel as Go Live. It is driven by CLI subcommands or a TUI, and keeps feature parity with the current app: codec toggle, resolution/FPS/bitrate, audio gain, health scoring, auto-reconnect, channel-move follow.

## Non-goals

- Windows (keep the traits open for it, but don't implement it).
- A GUI or tray. The core exposes a `Session` API so one can be added later.
- Bot-account support.

## Workspace layout (`rust/`)

| Crate | Role |
|---|---|
| `dstreamy-core` | Domain types (`StreamConfig`, `Codec`, `CaptureTarget`, `CropRect`), health scoring, metrics, config, `Session` state machine |
| `dstreamy-media` | `CaptureSource` trait, `EncodedVideoFrame` (Annex B, pts µs, keyframe), `AudioFrame`, Annex B utilities, `TestPatternSource` (openh264) |
| `dstreamy-capture-macos` | ScreenCaptureKit + VideoToolbox backend (`cfg(target_os = "macos")`) |
| `dstreamy-capture-linux` | Portal + PipeWire + GStreamer backend (`cfg(target_os = "linux")`) |
| `dstreamy-discord` | User-token gateway client + adapter over vendored `discord-stream-rs` |
| `dstreamy-mock-discord` | Fake gateway, voice WS, WebRTC answerer and DAVE peer; records and decodes what was received (test-only) |
| `dstreamy-cli` | `clap` subcommands + `ratatui` TUI; binary name `dstreamy` |
| `vendor/discord-stream-rs` | Vendored upstream, patched as needed |

## Glossary

Use these terms in code, issues and commits.

- **Session**: one Go Live stream lifecycle.
- **Target**: the window or display to capture.
- **Health**: Video and Audio stability scores.
- **Channel move**: the user is moved to another voice channel; patch `channelId`, don't rejoin (vault pitfall `discord-channel-move`).

## Global gates

These apply to every issue, on top of its own gates.

```
cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
docker run --rm -v "$PWD":/w -w /w/rust dstreamy-linux-ci cargo test --workspace     # Linux, image from rust/ci/Dockerfile
```

CI on the pushed `rust-rewrite` branch must be green: `gh run watch` on the latest run.

## Agent operating rules

1. **Order:** work through issues by number, respecting `Depends:`. Issues with satisfied deps may run in parallel worktrees.
2. **Done** means all gates pass. Then set `Status: done` and append evidence under `## Comments`: the commands run, the result, the commit SHA.
3. **No asking.** When something is ambiguous, pick the option most consistent with the ADR and this PRD. Record it as `Decision: …` in the issue's Comments; structural decisions get a new ADR in `docs/adr/`. Stop only when a gate is impossible without a human. Then set `Status: needs-info`, add the item to `MANUAL-CHECKLIST.md`, and move on to the next unblocked issue.
4. **Self-review instead of human review:** before each commit, run a reviewer subagent on the diff, fix confirmed findings, then re-run the gates.
5. **Git:** commit to `rust-rewrite` only, with Conventional Commits and no AI attribution. Push after each issue. Never touch `main`, never merge, never force-push. Don't stage `.claude/` or `CLAUDE.md` changes that predate this work.
6. **Forbidden:** real Discord tokens, any network call to `discord.com` / `discord.gg` / `gateway.discord.gg`, and `--no-verify`.
7. **Reference sources:** before implementing protocol details, read the actual source of `node_modules/@dank074/discord-video-stream` and `vendor/discord-stream-rs`. Don't guess.
8. **Lessons from the vault:** read `~/knowledge/projects/dstreamy/` pitfalls before touching related areas. Durable new learnings go to the vault per its `AGENTS.md`.
9. **Code comments:** default none. At most one line, and only for a non-obvious why, an invariant, an intentionally swallowed error or a workaround. No narration, no banners, no TODOs, no task references. Every subagent brief must include this rule.
10. **Tests:** port the existing Swift and TS tests as golden behaviour (`capture/Tests`, `src/core/*.test.ts`). Prefer in-process tests that need no permission, following the vault pattern `in-process-encoder-tests`.

## Human-only items

These are collected in `MANUAL-CHECKLIST.md` and never block other issues.

- Live Go Live against real Discord, with a burner account.
- macOS Screen Recording permission plus a real ScreenCaptureKit capture.
- The Wayland portal dialog and real PipeWire capture on a Linux desktop.
- Hardware encoders (VA-API, NVENC) on real GPUs.
- Final removal of the Swift and Bun code.
