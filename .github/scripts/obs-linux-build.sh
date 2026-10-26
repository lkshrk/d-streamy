#!/usr/bin/env bash
set -euo pipefail

start=$(date +%s)
git clone --depth 1 --branch "$OBS_TAG" --recurse-submodules --shallow-submodules \
  https://github.com/obsproject/obs-studio.git "$RUNNER_TEMP/obs-src"
cd "$RUNNER_TEMP/obs-src"
test "$(git rev-parse HEAD)" = "$OBS_COMMIT"
cmake --preset ubuntu \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_INSTALL_PREFIX="$OBS_PREFIX" \
  -DCMAKE_COMPILE_WARNING_AS_ERROR=OFF \
  -DENABLE_BROWSER=OFF -DENABLE_SCRIPTING=OFF -DENABLE_VLC=OFF \
  -DENABLE_NVENC=OFF -DENABLE_QSV11=OFF -DENABLE_NEW_MPEGTS_OUTPUT=OFF \
  -DENABLE_WEBRTC=OFF -DENABLE_AJA=OFF
cmake --build build_ubuntu --parallel
cmake --install build_ubuntu
echo "OBS build took $(($(date +%s) - start)) s" | tee "$OBS_PREFIX/build-time.txt"
