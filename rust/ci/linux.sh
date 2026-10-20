#!/usr/bin/env bash
# Usage: rust/ci/linux.sh [command...]   (default: full gate)
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
docker build -q -t dstreamy-linux-ci "$root/rust/ci" >/dev/null
cmd=("$@")
[ ${#cmd[@]} -eq 0 ] && cmd=(bash -c "cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace")
exec docker run --rm \
  -v "$root":/w -w /w/rust \
  -v dstreamy-cargo-registry:/usr/local/cargo/registry \
  -v dstreamy-linux-target:/w/rust/target \
  dstreamy-linux-ci "${cmd[@]}"
