#!/usr/bin/env bash
# Usage: ARCH=arm64|x86_64 OBS_PREFIX=... OBS_LOGS=... obs-macos-plugin-tests.sh, from the repository root.
set -euo pipefail

logs="$OBS_LOGS/$ARCH"
mkdir -p "$logs"
app="$OBS_PREFIX/OBS.app/Contents"
lipo -archs "$app/Frameworks/libobs.framework/libobs" | tee "$logs/obs-archs.log"
cp obs/build_macos/harness/RelWithDebInfo/obs-harness "$app/MacOS/"
plugin="$GITHUB_WORKSPACE/obs/build_macos/rundir/%module%.plugin/Contents"
arch "-$ARCH" bash obs/tests/rust-plugin.sh "$app/MacOS/obs-harness" "$plugin/MacOS" \
  "$plugin/Resources" "$logs/rust-plugin" --graphics "$app/Frameworks/libobs-opengl.dylib" |
  tee "$logs/rust-plugin.log"
