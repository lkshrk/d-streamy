#!/usr/bin/env bash
# Usage: OBS_PREFIX=... obs-linux-check-install.sh; exports LD_LIBRARY_PATH through GITHUB_ENV.
set -euo pipefail

cat "$OBS_PREFIX/build-time.txt" || true
libdir=$(dirname "$(find "$OBS_PREFIX" -name 'libobs.so.*' | head -n 1)")
echo "LD_LIBRARY_PATH=$libdir" >> "$GITHUB_ENV"
for m in obs-x264 obs-ffmpeg obs-outputs image-source obs-websocket frontend-tools; do
  test -f "$libdir/obs-plugins/$m.so" || { echo "missing module $m"; exit 1; }
done
test -x "$OBS_PREFIX/bin/obs"
