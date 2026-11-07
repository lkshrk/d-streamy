#!/usr/bin/env bash
set -euo pipefail

start=$(date +%s)
if [ -n "${FFMPEG_TAG:-}" ]; then
  git clone --depth 1 --branch "$FFMPEG_TAG" https://github.com/FFmpeg/FFmpeg.git "$RUNNER_TEMP/ffmpeg-src"
  test "$(git -C "$RUNNER_TEMP/ffmpeg-src" rev-parse HEAD)" = "$FFMPEG_COMMIT"
  (
    cd "$RUNNER_TEMP/ffmpeg-src"
    ./configure --prefix="$OBS_PREFIX" --libdir="$OBS_PREFIX/lib" --enable-shared --disable-static \
      --disable-programs --disable-doc --disable-debug --enable-gpl --enable-libx264
    make -j"$(nproc)"
    make install
  )
  export PKG_CONFIG_PATH="$OBS_PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
git clone --depth 1 --branch "$OBS_TAG" --recurse-submodules --shallow-submodules \
  https://github.com/obsproject/obs-studio.git "$RUNNER_TEMP/obs-src"
cd "$RUNNER_TEMP/obs-src"
test "$(git rev-parse HEAD)" = "$OBS_COMMIT"
cmake --preset ubuntu \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_INSTALL_PREFIX="$OBS_PREFIX" \
  -DCMAKE_PREFIX_PATH="$OBS_PREFIX" \
  -DCMAKE_COMPILE_WARNING_AS_ERROR=OFF \
  -DENABLE_BROWSER=OFF -DENABLE_SCRIPTING=OFF -DENABLE_VLC=OFF \
  -DENABLE_NVENC=OFF -DENABLE_QSV11=OFF -DENABLE_NEW_MPEGTS_OUTPUT=OFF \
  -DENABLE_WEBRTC=OFF -DENABLE_AJA=OFF
cmake --build build_ubuntu --parallel
cmake --install build_ubuntu
echo "OBS build took $(($(date +%s) - start)) s" | tee "$OBS_PREFIX/build-time.txt"
