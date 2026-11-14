#!/usr/bin/env bash
# Usage: rust/ci/linux.sh [command...]   (default: full gate)
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/rust/rust-toolchain.toml")"
docker build -q --build-arg RUST_TOOLCHAIN="$toolchain" -t dstreamy-linux-ci "$root/rust/ci" >/dev/null
cmd=("$@")
[ ${#cmd[@]} -eq 0 ] && cmd=(bash -c "cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked")
exec docker run --rm -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" -e CARGO_INCREMENTAL=0 \
  -v "$root":/w -w /w/rust \
  -v dstreamy-cargo-registry:/usr/local/cargo/registry \
  -v "${DSTREAMY_TARGET_VOLUME:-dstreamy-linux-target}":/w/rust/target \
  dstreamy-linux-ci "${cmd[@]}"
