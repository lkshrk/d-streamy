# 01 — workspace and ci

Status: ready-for-agent
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
