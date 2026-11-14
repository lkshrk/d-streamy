# Contributing

Contributions are welcome: bug reports, fixes and improvements to capture, the service, the OBS plugin and the macOS app.

Read the [disclaimer](README.md#disclaimer) first. Never put a Discord token, or an account's `token` file, into an issue, log, fixture or commit, and remove ids you don't want public.

## Reporting issues

Include:

- your OS and its version;
- which frontend you used: `dstreamy stream` or `tui`, the OBS plugin or the macOS app;
- the OBS version, if OBS was involved;
- the relevant lines of the logs, with tokens and ids removed: for the OBS plugin, OBS's log (Help > Log Files) and serve's `dstreamy.log` from the config directory; for the macOS app and `tui`, `dstreamy.log`; for `dstreamy stream`, its stderr with `-vv`.

## Before you change code

- Read the [overview](docs/overview.md) and [architecture](docs/architecture.md), then the chapter for the part you are changing in [docs/components/](docs/components/).
- Use the words in [CONTEXT.md](CONTEXT.md): "account" for a saved login, "server" for a guild in user-facing text, "service" for `dstreamy serve`, "daemon" only for the macOS app's child.
- The [ADRs](docs/adr/) are binding. A change that contradicts one needs a new ADR or an amendment.
- [docs/specs.md](docs/specs.md) pins every external behaviour the code relies on. A behaviour with no sourced row there is neither relied on nor mocked: add the row before the code.
- Golden fixtures are contracts: `rust/crates/dstreamy-cli/tests/fixtures/`, `rust/crates/dstreamy-ipc/tests/fixtures/` and `obs/tests/traces/`. Changing one needs a protocol bump or new provenance. The regenerate switches described in the fixture READMEs only write a change you intended.
- Test-only features (`test-hooks`, `test-pattern`, `openh264`, `test-support`, `test-rtp-start`, and the CMake option `DSTREAMY_PLUGIN_TEST_HOOKS`) must never reach a shipped build. Rust CI and the release workflow check the dependency graph, and `obs/cmake/common/check_test_hooks.cmake` scans the built plugin.
- The OBS plugin runs on stock OBS. Never patch or fork libobs.

## Setting up

- The Rust toolchain is pinned in `rust/rust-toolchain.toml`; rustup installs it when you run cargo in `rust/`.
- Linux needs `clang libclang-dev pkg-config libpipewire-0.3-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`, and the GStreamer runtime plugins (base, good, bad, ugly) to run the tests. Browser login also needs `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev`.
- macOS needs Xcode 27.1.
- The OBS plugin needs an OBS 32.2.2 build to compile against. On Linux, `.github/scripts/obs-linux-build.sh` builds one the way CI does; the macOS and Windows presets download it.

## Checks to run

Rust, from `rust/`:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --locked -p dstreamy-test-producer --bin mock-producer
export DSTREAMY_BIN="$PWD/target/debug/dstreamy" DSTREAMY_MOCK_PRODUCER="$PWD/target/debug/mock-producer"
cargo nextest run --workspace --locked --profile ci -E 'not binary_id(dstreamy-cli::trace_replay)'
DSTREAMY_TRACE_REPLAY_STRICT=1 cargo test --workspace --locked --test trace_replay
cargo test --workspace --locked --doc
```

`trace_replay` must run under `cargo test`, never nextest. If you touch browser login, also run clippy and the tests with `-p dstreamy-cli --features browser-login`.

OBS plugin, from `obs/`, with your OBS build's prefix in `OBS_PREFIX`:

```sh
cmake --preset ubuntu-ci-x86_64 -DCMAKE_PREFIX_PATH="$OBS_PREFIX" -DDSTREAMY_PLUGIN_TEST_HOOKS=ON
cmake --build --preset ubuntu-ci-x86_64
xvfb-run -a -s "-screen 0 1280x720x24" ctest --preset ubuntu-ci-x86_64
```

Then, from `rust/crates/dstreamy-obs` with `DSTREAMY_OBS_CONFIG` set to the generated `obs/build_x86_64/obs-config-RelWithDebInfo.txt`: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` (also with `--features test-hooks`) and `cargo test --workspace --locked` under `xvfb-run`.

macOS app: build `dstreamy` with test hooks and the mock Discord in `rust/` (`cargo build --locked -p dstreamy-cli --features test-hooks -p dstreamy-mock-discord`), then run `DSTREAMY_INTEGRATION=1 swift test` at the repository root.

The workflows in `.github/workflows/` have the exact steps per platform, including the Windows jobs. [Build, test and release](docs/components/build-test-release.md) explains each job.

## Releasing

One `vX.Y.Z` tag releases everything: the CLI for macOS, Linux and Windows, the signed macOS app, and the OBS plugin for Linux, macOS and Windows, all on one GitHub release, plus the Homebrew formula and cask in `lkshrk/homebrew-tap` and the signed pacman repository on GitHub Pages.

- `scripts/tag-release.sh major|minor|patch|vX.Y.Z` sets the version in both Cargo workspaces (`rust/` and `rust/crates/dstreamy-obs/`), their `Cargo.lock` files, `rust/fuzz/Cargo.lock` and `obs/buildspec.json`, commits, tags and pushes.
- Publishing waits for CI, Rust CI, OBS and Fuzz to pass on the tagged commit.
- To rehearse, push to the `release-rehearsal` branch: `git push -f origin HEAD:release-rehearsal`. It builds and checks everything and publishes nothing.

## Pull requests

- Fork, branch from `main`, and keep each pull request to one change.
- Add tests for new behaviour.
- Use [Conventional Commits](https://www.conventionalcommits.org/) with a scope where it helps, for example `fix(serve): …` or `feat(obs): …`.
- Write plain, short sentences in docs, help text and messages.
