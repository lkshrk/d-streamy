#!/usr/bin/env bash
# Usage: check-universal.sh FILE — fails unless FILE holds both an arm64 and an x86_64 slice.
set -euo pipefail
# Xcode 27's lipo rejects `FILE -verify_arch ARCH...`; -archs works on every Xcode.
archs=" $(lipo -archs "$1") "
[[ $archs == *" arm64 "* && $archs == *" x86_64 "* ]] || { echo "$1 is not universal (archs:$archs)" >&2; exit 1; }
