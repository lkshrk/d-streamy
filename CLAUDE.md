# D-Streamy

One binary, `dstreamy`, streams a window, a display or OBS's output to Discord Go Live. Decisions live in `docs/adr/` and are binding; `docs/specs.md` pins every external behaviour the service and the OBS plugin rely on, and nothing without a source there is relied on or mocked. `CONTEXT.md` is the glossary; use its terms.

## Build units

- `rust/`: the Cargo workspace. `dstreamy-cli` builds `dstreamy` (CLI, TUI, `daemon`, `serve`, `service`); `dstreamy-core`, `dstreamy-media`, `dstreamy-discord`, `dstreamy-ipc`, the two capture crates, `dstreamy-mock-discord`, `dstreamy-test-producer` and `dstreamy-obs-producer` (the plugin's libobs-free half) are libraries. `rust/vendor/` holds the vendored Discord transport.
- `rust/crates/dstreamy-obs` with `dstreamy-obs-sys`: the OBS plugin, its own workspace because it links libobs. Built only through `obs/`; `cargo test --workspace` in `rust/` never touches it.
- `obs/`: the obs-plugintemplate CMake project (presets in `CMakePresets.json`), the Qt panel in `obs/ui/`, the headless harness in `obs/harness/` and recorded traces in `obs/tests/traces/`. OBS 32.2.2 is the build baseline (`obs/buildspec.json`); Linux plugin CI also defines a 33.0.1 compatibility job. macOS and Windows use the baseline.
- `App/`: the SwiftUI menu bar app, a frontend over `dstreamy daemon` (ADR 0003).

## Build and test, as CI does

Rust (`.github/workflows/rust-ci.yml`), from `rust/`. The workflow defines macOS `xcode-27`, Linux `ubuntu-24.04` and `ubuntu-24.04-arm`; cross-user, browser-smoke and isolated systemd jobs have additional setup in the workflow.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --locked -p dstreamy-cli -p dstreamy-test-producer --features dstreamy-cli/test-pattern --bin dstreamy --bin mock-producer
export DSTREAMY_BIN="$PWD/target/debug/dstreamy"
export DSTREAMY_MOCK_PRODUCER="$PWD/target/debug/mock-producer"
cargo nextest run --workspace --locked --profile ci -E 'not binary_id(dstreamy-cli::trace_replay)'
DSTREAMY_TRACE_REPLAY_STRICT=1 cargo test --workspace --locked --test trace_replay
cargo test --workspace --locked --doc
cargo clippy -p dstreamy-cli --features browser-login --all-targets --locked -- -D warnings
cargo test -p dstreamy-cli --features browser-login --locked --lib --test browser_login --test cli
```

The commands above prepare required binary paths and exclude trace replay from nextest. For public documentation run `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --locked --no-deps` in each Cargo workspace after its dependencies are configured. This does not prove every public item has documentation; method, field and variant coverage remains pending.

OBS plugin (`.github/workflows/obs-ci.yml`), against an OBS 32.2.2 build at `$OBS_PREFIX`:

```sh
cd obs
cmake --preset ubuntu-ci-x86_64 -DCMAKE_PREFIX_PATH="$OBS_PREFIX" -DDSTREAMY_PLUGIN_TEST_HOOKS=ON
cmake --build --preset ubuntu-ci-x86_64
xvfb-run -a -s "-screen 0 1280x720x24" ctest --preset ubuntu-ci-x86_64
cd ../rust/crates/dstreamy-obs
export DSTREAMY_OBS_CONFIG="$PWD/../../../obs/build_x86_64/obs-config-RelWithDebInfo.txt"
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features test-hooks -- -D warnings
xvfb-run -a -s "-screen 0 1280x720x24" cargo test --workspace --locked
```

On macOS use `macos-ci`, then `ctest --test-dir build_macos -C RelWithDebInfo -R dstreamy-ui --output-on-failure`; no macOS test preset exists. Set `DSTREAMY_OBS_CONFIG` to that build's generated config. The workflow checks both target slices and runs load tests separately.

macOS app (`.github/workflows/ci.yml`): `swift test` after `cargo build --locked -p dstreamy-cli --features test-pattern -p dstreamy-mock-discord` in `rust/`; `bash scripts/build.sh` makes `bin/D-Streamy.app`.

## Rules

- Golden fixtures are contracts: `rust/crates/dstreamy-cli/tests/fixtures/`, `rust/crates/dstreamy-ipc/tests/fixtures/` and `obs/tests/traces/`. Changing one needs a protocol bump or new provenance, not a regenerate.
- Stock OBS only: the plugin runs on users' unpatched OBS. Never patch or fork libobs.
- Comments: almost none, one line at most; `///` docs on public items say what and why.
- Vocabulary: "account" for a saved login, "server" for a guild in user-facing text, "service" for `dstreamy serve`, "daemon" only for the macOS app's stdio child.

## Agent skills

### Domain docs

Single-context layout — one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
