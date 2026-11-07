# D-Streamy for OBS

The OBS plugin: `obs-dstreamy`, a Rust `cdylib` built by this CMake project, plus the Qt status-bar panel `obs-dstreamy-ui` in `ui/`. It sends OBS's encoded H.264 and Opus to a running `dstreamy serve` over a local socket; it holds no Discord code and no token.

Install, usage and the build commands are in the [OBS plugin section of the main README](../README.md#obs-plugin). The design is [ADR 0004](../docs/adr/0004-persistent-service-and-external-producers.md) and [ADR 0007](../docs/adr/0007-obs-status-bar-panel.md); every OBS behaviour the plugin relies on is pinned in [docs/specs.md](../docs/specs.md).

Layout:

- `CMakeLists.txt`, `CMakePresets.json`, `cmake/`: the `obs-plugintemplate` build, with `cmake/common/rust_plugin.cmake` driving cargo for `../rust/crates/dstreamy-obs`.
- `ui/`: the C++/Qt panel and settings dialog, and their tests.
- `harness/`: `obs-harness`, a headless libobs host that records encoder traces.
- `tests/traces/`: recorded OBS packets with a `PROVENANCE.md` each; golden inputs for the Rust tests.
- `buildspec.json`: the pinned OBS version (32.2.2) and its dependencies.


The plugin builds against stock OBS Studio. Do not patch libobs or depend on unreleased host changes.

Release packaging is defined in [release-obs.yml](../.github/workflows/release-obs.yml) for `obs-v*` tags, with a nonpublishing rehearsal by default. A workflow definition is not proof that packages were published or signing ran. The [install layouts](../README.md#obs-plugin) distinguish Linux prefix tarballs, macOS bundles and Windows folder zips; Windows service transport remains unsupported. Linux plugin CI includes 32.2.2 and 33.0.1; the cross-platform build baseline remains 32.2.2.

## Dependency pins

When changing the OBS tag in `buildspec.json`, compare the tag's upstream `buildspec.json` before updating dependencies. Keep the `qt6` and `prebuilt` versions equal to the versions used by that OBS release. A helper linked against a newer Qt minor can fail to load in the user's OBS even when the Rust module itself loads.

The Qt helper is optional at runtime. If it cannot load, Tools > D-Streamy opens the stock properties dialog instead.

## Trace fixtures

OBS CI records fresh traces and compares their metadata and packet counts with `tests/traces` before running the spec checks against the new captures. Drift produces a warning and a step summary. Inspect the uploaded fresh traces and re-record committed fixtures after an OBS, x264, FFmpeg or runner-image change. The provenance file beside each trace records its original environment.

Captured encoder-settings fixtures must match the complete defaults map and property-name set. Source-derived fixtures remain partial until captured on supported hardware, and the checker lists them in the step summary.
