# 13 — packaging and release

Status: ready-for-agent
Depends: 09,11,12

## Scope

Release builds for aarch64/x86_64 macOS and x86_64/aarch64 Linux via GitHub Actions on tags `rs-v*` (don't create tags — workflow only). Update `README.md` with new usage section (keep old section until removal). Homebrew formula draft under `Casks/` or `Formula/` (not published).

## Gates

- Workflow passes `actionlint`
- Local `cargo build --release` on macOS + in container
- README build/usage commands verified by running them
- Global gates from PRD

## Comments
