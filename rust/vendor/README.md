# Vendored crates

## discord-stream-rs

| | |
|---|---|
| Upstream | https://github.com/Tky567/stream-discord-rs |
| Upstream version | 0.1.3 |
| Upstream commit | `50a402c3da9f14755ef622f27387db502d8300f9` (from `.cargo_vcs_info.json` in the published crate) |
| Source | https://crates.io/api/v1/crates/discord-stream-rs/0.1.3/download |
| SHA-256 of the `.crate` | `228ddf36e24d1826c6f93d60b9cc0fb9def2639f28a08063c9bf493d454cc15c` |
| Local version | `0.1.3+dstreamy`, a fork; `discord-stream-rs/VENDOR.md` lists every local change by commit |
| License | MIT, see `discord-stream-rs/LICENSE` |

Imported from the published crate. `Cargo.toml` is the author's `Cargo.toml.orig`
(the registry-normalised manifest and `Cargo.lock` were dropped). The fork has grown
well past a patch set, so an upstream sync is not planned; changes land as normal
commits and `VENDOR.md` is the changelog against the import.

## webrtc

| | |
|---|---|
| Upstream | https://github.com/webrtc-rs/webrtc (crate `webrtc` in that workspace) |
| Version | 0.14.0 |
| Upstream commit | `a1f8f1919235d8452835852e018efd654f2f8366` (from `.cargo_vcs_info.json`) |
| Source | https://crates.io/api/v1/crates/webrtc/0.14.0/download |
| SHA-256 of the `.crate` | `08fd686c0920ac08f3a57eacc48e31f0e4ca1ffefba4478784606f78c14e83ad` (same as the `Cargo.lock` checksum it replaces) |
| License | MIT OR Apache-2.0; `LICENSE-MIT` and `LICENSE-APACHE` fetched from the upstream commit, since the published crate ships none |

Wired in through `[patch.crates-io]` and excluded from the workspace, so it is built
as a dependency only (no clippy/fmt/tests). `Cargo.toml` is the registry-normalised
manifest from the published crate. It is vendored for one patch: an RTP sender must be
able to use the SSRC Discord assigns in voice READY. See ADR 0002.

## Reference

The protocol reference is `@dank074/discord-video-stream` 6.0.0.
