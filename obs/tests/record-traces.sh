#!/usr/bin/env bash
# usage: record-traces.sh <obs-harness> <obs-trace-check> <out-dir>
set -euo pipefail

harness=$1
check=$2
out=$3
seconds=3
mkdir -p "$out"

record() {
  local name=$1 check_fps=$2
  shift 2
  echo "=== $name"
  "$harness" --seconds "$seconds" --bitrate 500 --trace-dir "$out/$name" --note "$name" "$@"
  if [[ $check_fps != - ]]; then
    "$check" "$out/$name" --fps "$check_fps" --seconds "$seconds"
  fi
}

for fps in 30 60; do
  for profile in baseline main; do
    record "linux-x264-$profile-bf0-keyint2-${fps}fps" "$fps" \
      --profile "$profile" --bf 0 --keyint-sec 2 --fps "$fps"
  done
done
record linux-x264-main-bf2-keyint2-30fps - --profile main --bf 2 --keyint-sec 2
record linux-x264-main-bfunset-keyint2-30fps - --profile main --keyint-sec 2
record linux-x264-main-bf0-keyint0-30fps - --profile main --bf 0 --keyint-sec 0
record linux-ffmpeg_opus-defaults 30 --profile main --bf 0 --keyint-sec 2 --audio-encoder ffmpeg_opus
record linux-ffmpeg_aac-defaults - --profile main --bf 0 --keyint-sec 2 --audio-encoder ffmpeg_aac

du -sh "$out"/*
