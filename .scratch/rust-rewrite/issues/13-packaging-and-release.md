# 13 — packaging and release

Status: done
Depends: 09,11,12

## Scope

Release builds for aarch64/x86_64 macOS and x86_64/aarch64 Linux via GitHub Actions on tags `rs-v*` (don't create tags — workflow only). Update `README.md` with new usage section (keep old section until removal). Homebrew formula draft under `Casks/` or `Formula/` (not published).

## Gates

- Workflow passes `actionlint`
- Local `cargo build --release` on macOS + in container
- README build/usage commands verified by running them
- Global gates from PRD

## Comments
- Note: `screencapturekit` 11 depends on `apple-metal`, which needs the Xcode 26 SDK (`MTLTensor*`). macOS release jobs must run on `macos-26` (as `rust-macos` CI does since 998aa8e); macos-15 fails with `cannot find 'MTLTensorDataType' in scope`.
- Note: the binary crate is `rust/crates/dstreamy-cli` (bin `dstreamy`); its `build.rs` already sets the Swift rpath on macOS. Issue 10 (TUI) edits that crate in parallel, so don't touch `rust/crates/dstreamy-cli/src/`. Any `Cargo.toml` change there (e.g. release profile metadata) goes in the workspace `rust/Cargo.toml` instead.
- Decision: the release workflow is a separate file, `.github/workflows/release-rust.yml`, triggered by `push` of `rs-v*` tags. The Swift `release.yml` only acts on `vX.Y.Z` tags, so the two never collide. A `version` job fails the run unless the tag equals `rs-v<dstreamy-cli version>`, and a `check` job (fmt, clippy, tests on macos-26 and ubuntu-24.04) gates the builds.
- Decision: both macOS targets build on `macos-26`; x86_64 is cross-compiled (screencapturekit's build.rs maps the Cargo arch to the Swift triple). `MACOSX_DEPLOYMENT_TARGET=14.0`, matching the app's macOS 14 floor. The x86_64 binary is smoke-run under Rosetta, installed with `softwareupdate --install-rosetta` (idempotent if the image already has it).
- Decision: Linux aarch64 builds natively on `ubuntu-24.04-arm` (free for this public repository) rather than cross-compiling, because GStreamer/glib are linked dynamically through pkg-config and a cross sysroot would be fragile. Linux binaries need glibc 2.39+ (Ubuntu 24.04, Debian 13) and the distro GStreamer packages listed in the README.
- Decision: release is created with `--prerelease --latest=false`, so the Swift app's release stays "latest" until issue 14 signs off the rewrite. Each archive is `dstreamy-<version>-<target>.tar.gz` (binary + README) with a `.sha256` next to it; the release job prints all hashes into the run summary.
- Decision: `[profile.release]` in `rust/Cargo.toml`: `lto = "thin"`, `strip = "symbols"` (macOS arm64 binary 14.8 MB, Linux aarch64 18.2 MB).
- Decision: the Homebrew formula draft `Formula/dstreamy.rb` is macOS-only (`depends_on macos: :sonoma`); Homebrew on Linux would not supply the system GStreamer/PipeWire plugins the binary needs. Its sha256 values are placeholders until a real `rs-v*` release exists (see MANUAL-CHECKLIST). It is not copied to the tap.
- Evidence (2026-10-21):
  - `actionlint` 1.7.12 (Docker `rhysd/actionlint`) on `.github/workflows/release-rust.yml` → exit 0. Pre-existing shellcheck notes in `ci.yml`/`release.yml` are untouched.
  - `brew style Formula/dstreamy.rb` → no offenses; `ruby -c` → Syntax OK.
  - macOS: `cd rust && cargo build --release` → ok; `target/release/dstreamy --version` → `dstreamy 0.1.0`; `otool -L` shows only system frameworks and `/usr/lib/swift` (rpath `/usr/lib/swift` present). `MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release --locked -p dstreamy-cli --target x86_64-apple-darwin` → ok, Mach-O x86_64, minos 14.0 (no Rosetta on this host, so not run locally).
  - Container (aarch64 Linux, Debian trixie): `cargo build --release --locked` on a clean snapshot of HEAD → ok; `ldd` shows GStreamer/glib/libssl3 only; `dstreamy --version` → `dstreamy 0.1.0`. Then `cargo clean --release` in the container (1.3 GiB freed, volume back to 7.4G free). The live tree was not used because issue 10's uncommitted work did not compile at the time.
  - README install steps reproduced locally: packaged the arm64 binary exactly like the workflow, `shasum -a 256 -c` → OK, `tar -xzf`, `install -m 755`, `--version` → ok. Usage commands run with an empty config dir: `guilds` and `stream --target test-pattern` → `not logged in` (exit 1, no network), `login` with empty stdin → `no token given on stdin`, `targets` → lists displays and windows.
  - Global gates on HEAD + this change: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → all ok on macOS; same in the Linux container (`CARGO_BUILD_JOBS=2`) → all ok.
  - Not verifiable without a tag: the GitHub-hosted runs themselves (see MANUAL-CHECKLIST).
- Commit: f20f40f (workflow, formula, README, release profile).
