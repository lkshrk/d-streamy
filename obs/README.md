# D-Streamy for OBS

The CMake project that builds the OBS plugin: `obs-dstreamy`, a Rust `cdylib` from [`rust/crates/dstreamy-obs`](../rust/crates/dstreamy-obs), and `obs-dstreamy-ui`, the C++/Qt status-bar panel in `ui/`. The plugin sends OBS's encoded H.264 and Opus to `dstreamy serve`; it holds no Discord code and no token.

- Installing and using it: the [README](../README.md#obs-plugin).
- How it works: [OBS plugin](../docs/components/obs-plugin.md).
- Building and testing it: [Build, test and release](../docs/components/build-test-release.md).

## Layout

- `CMakeLists.txt`, `CMakePresets.json`, `cmake/`: the obs-plugintemplate build. `cmake/common/rust_plugin.cmake` drives cargo for the plugin crate, and `cmake/common/check_test_hooks.cmake` checks a built plugin for test hooks.
- `ui/`: the Qt panel library and its tests.
- `harness/`: `obs-harness`, a headless libobs host used by the tests and to record encoder traces.
- `tests/traces/`: recorded OBS packets, each with a `PROVENANCE.md`. They are golden inputs for the Rust tests.
- `tests/encoder-settings/`: encoder default fixtures.
- `buildspec.json`: the pinned OBS version (32.2.2), its dependencies and the plugin version.

Build with a preset per platform: `cmake --preset ubuntu-x86_64`, `macos` or `windows-x64`, then `cmake --build --preset <the same>`. The Linux presets also need `-DCMAKE_PREFIX_PATH=<OBS prefix>`. CI uses `ubuntu-ci-x86_64`, `macos-ci` and `windows-ci-x64`; `-DDSTREAMY_PLUGIN_TEST_HOOKS=ON` builds the test hooks in. A release build must never have them.

The plugin builds against stock OBS Studio. Don't patch libobs or depend on unreleased OBS changes.

## Dependency pins

When changing the OBS tag in `buildspec.json`, compare it with that tag's upstream `buildspec.json` first. Keep the `qt6` and `prebuilt` versions equal to the ones that OBS release uses. A panel library linked against a newer Qt minor fails to load in the user's OBS even when the plugin module itself loads.

The panel library is optional at runtime. If it can't load, Tools > D-Streamy… opens OBS's stock properties dialog instead.

## Trace fixtures

OBS CI records fresh traces and compares their metadata and packet counts with `tests/traces` before running the spec checks on the new captures. Drift produces a warning and a step summary. After an OBS, x264, FFmpeg or runner-image change, inspect the uploaded fresh traces and re-record the committed fixtures. The `PROVENANCE.md` beside each trace records the environment it came from.

Captured encoder-settings fixtures must match the complete defaults map and property-name set. Fixtures derived from source stay partial until captured on supported hardware, and the checker lists them in the step summary.
