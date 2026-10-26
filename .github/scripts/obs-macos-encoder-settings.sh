#!/usr/bin/env bash
# Usage: ARCH=arm64|x86_64 OBS_PREFIX=... OBS_LOGS=... obs-macos-encoder-settings.sh, from the repository root.
set -euo pipefail

logs="$OBS_LOGS/$ARCH"
mkdir -p "$logs"
app="$OBS_PREFIX/OBS.app/Contents"
dump="$RUNNER_TEMP/encoder-settings-$ARCH"
arch "-$ARCH" "$app/MacOS/obs-harness" --graphics "$app/Frameworks/libobs-opengl.dylib" \
  --modules image-source,obs-x264,obs-ffmpeg,mac-videotoolbox \
  --dump-encoder-settings "$dump" | tee "$logs/encoder-settings.log"
python3 obs/tests/check-encoder-settings.py "$dump" obs/tests/encoder-settings \
  "the obs-encoder-settings-macos-$ARCH artifact of this run" obs_x264
