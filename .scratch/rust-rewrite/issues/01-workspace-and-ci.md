# 01 — workspace and ci

Status: done
Depends: —

## Scope

Scaffold the `rust/` Cargo workspace with all crates from the PRD as empty libs/bin; `rust-toolchain.toml` (stable, edition 2024); workspace lints (clippy pedantic off, `-D warnings` in CI).
`rust/ci/Dockerfile` (Debian stable + rustup + libpipewire-0.3-dev + libgstreamer1.0-dev + gst plugins base/good/bad/ugly + libclang + libopus-dev), tagged `dstreamy-linux-ci`.
Extend `.github/workflows/ci.yml` with jobs `rust-macos` (macos-15) and `rust-linux` (ubuntu-latest using the same apt deps): fmt, clippy, test. Keep existing jobs untouched.

## Gates

- `cargo build --workspace` ok on macOS
- `docker build -t dstreamy-linux-ci rust/ci && docker run ... cargo test --workspace` ok
- CI green on both new jobs after push
- Global gates from PRD

## Comments
- Evidence (2026-10-20): macOS `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` ok; `rust/ci/linux.sh` ok (Debian trixie image, arm64 local); CI run 39502849645 on 2ff2ce6: rust-macos, rust-linux, build-test all success.
- Decision: CI `push` trigger now also covers `rust-rewrite`, otherwise branch pushes get no CI.
- Decision: `rust/ci/linux.sh` takes `DSTREAMY_TARGET_VOLUME` so parallel worktrees don't share one Linux target dir.
- Decision: no `license` field in manifests; repo has no LICENSE file.
- Review round 2 (D), 2026-10-21: the Rust jobs moved from `ci.yml` to `rust-ci.yml` ("Rust CI", same triggers), so the Swift `release.yml` `workflow_run` on "CI" no longer waits for Rust. `rust-toolchain.toml` pins 1.99.0 (minimal profile + rustfmt, clippy); the workflows run `rustup toolchain install` in `rust/` instead of `dtolnay/rust-toolchain@stable`. All actions are pinned to commit SHAs with `# vX.Y.Z` comments, `.github/dependabot.yml` updates them weekly, `ci.yml` and `rust-ci.yml` have top-level `permissions: contents: read` (`release-rust.yml` already had it and elevates only its release job; the Swift `release.yml` is unchanged), the Linux job runs on `ubuntu-24.04`, and clippy/test use `--locked` in CI, the release workflow and `rust/ci/linux.sh`.
- `rust/ci/Dockerfile` takes `ARG RUST_TOOLCHAIN` (default 1.99.0) and `rust/ci/linux.sh` passes the channel read from `rust-toolchain.toml`, so the image already has the pinned toolchain. Not built locally (Docker is down).
