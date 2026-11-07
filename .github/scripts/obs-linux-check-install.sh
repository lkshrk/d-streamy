#!/usr/bin/env bash
# Exports LD_LIBRARY_PATH for later steps through GITHUB_ENV.
set -euo pipefail

cat "$OBS_PREFIX/build-time.txt" || true
libdir=$(dirname "$(find "$OBS_PREFIX" -name 'libobs.so.*' | head -n 1)")
# A source-built FFmpeg lands in $OBS_PREFIX/lib, next to or apart from libobs.
echo "LD_LIBRARY_PATH=$libdir:$OBS_PREFIX/lib" >> "$GITHUB_ENV"
# OBS 32 installs its modules under obs-plugins, OBS 33 under obs-modules/core.
for m in obs-x264 obs-ffmpeg obs-outputs image-source obs-websocket frontend-tools; do
  test -f "$libdir/obs-plugins/$m.so" || test -f "$libdir/obs-modules/core/$m.so" ||
    { echo "missing module $m"; exit 1; }
done
test -x "$OBS_PREFIX/bin/obs"
