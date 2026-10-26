#!/usr/bin/env bash
set -euo pipefail

start=$(date +%s)
src="$RUNNER_TEMP/obs-src-$ARCH"
git clone --depth 1 --branch "$OBS_TAG" --recurse-submodules --shallow-submodules \
  https://github.com/obsproject/obs-studio.git "$src"
cd "$src"
test "$(git rev-parse HEAD)" = "$OBS_COMMIT"
cmake --preset macos \
  -DCMAKE_OSX_ARCHITECTURES="$ARCH" \
  -DCMAKE_COMPILE_WARNING_AS_ERROR=OFF \
  -DENABLE_BROWSER=OFF -DENABLE_SCRIPTING=OFF -DENABLE_VIRTUALCAM=OFF -DENABLE_AJA=OFF \
  -DSPARKLE_APPCAST_URL= -DSPARKLE_PUBLIC_KEY=
cmake --build build_macos --config RelWithDebInfo --target obs-studio -- -quiet 2>&1 |
  xcbeautify --quiet --renderer github-actions
mkdir -p "$OBS_PREFIX"
cmake --install build_macos --config RelWithDebInfo --prefix "$OBS_PREFIX"
cd "$RUNNER_TEMP"
rm -rf "$src"
echo "OBS build took $(($(date +%s) - start)) s" | tee "$OBS_PREFIX/build-time.txt"
