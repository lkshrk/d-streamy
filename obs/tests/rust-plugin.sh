#!/usr/bin/env bash
# usage: rust-plugin.sh <obs-harness> <plugin-bin-path> <plugin-data-path> <log-dir> [harness args...] (macOS paths: %module% patterns)
set -euo pipefail

harness=$1
bin=$2
data=$3
logs=$4
shift 4
mkdir -p "$logs"
modules="image-source,obs-x264,obs-ffmpeg,obs-outputs,obs-dstreamy"
seconds=3
min_video=$((seconds * 30 / 2))
min_audio=$((seconds * 50 / 2))

run() {
  local name=$1
  shift
  echo "=== $name"
  local rc=0
  "$harness" --seconds "$seconds" --output dstreamy_output --video-encoder obs_x264 --audio-encoder ffmpeg_opus \
    --plugin-bin "$bin" --plugin-data "$data" --modules "$modules" --trace-dir "$logs/$name-trace" "$@" \
    >"$logs/$name.log" 2>&1 || rc=$?
  cat "$logs/$name.log"
  if [[ $rc != 0 ]]; then
    echo "FAIL $name: harness exited with $rc"
    return 1
  fi
}

expect() {
  local name=$1 pattern=$2
  if ! grep -Eq -- "$pattern" "$logs/$name.log"; then
    echo "FAIL $name: no line matching: $pattern"
    return 1
  fi
}

expect_packets() {
  local name=$1 panics=$2 line video audio
  line=$(grep -E "^HARNESS output=dstreamy_output .* panics=$panics\$" "$logs/$name.log") || {
    echo "FAIL $name: no stats line with panics=$panics"
    return 1
  }
  video=$(sed -E 's/.* video=([0-9]+) .*/\1/' <<<"$line")
  audio=$(sed -E 's/.* audio=([0-9]+) .*/\1/' <<<"$line")
  if ((video < min_video || audio < min_audio)); then
    echo "FAIL $name: video=$video (min $min_video), audio=$audio (min $min_audio)"
    return 1
  fi
}

run load "$@"
expect load '\[obs-dstreamy\] loaded \(version '
expect_packets load 0
expect load 'HARNESS shutdown libobs_leaks=0'
expect load 'HARNESS result=PASS'

run panic --output-settings '{"panic_at_packet": 5}' --expect-panics 1 "$@"
expect panic '\[obs-dstreamy\] panic caught in encoded_packet: test panic at packet 5'
expect_packets panic 1
expect panic 'HARNESS shutdown libobs_leaks=0'
expect panic 'HARNESS result=PASS'

echo "rust-plugin: PASS"
