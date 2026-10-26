#!/usr/bin/env bash
# usage: rust-plugin.sh <obs-harness> <plugin-bin-path> <plugin-data-path> <log-dir> [harness args...] (macOS paths: %module% patterns)
# Runs without a D-Streamy service, so only start errors; streaming through serve is rust/crates/dstreamy-cli/tests/obs_plugin.rs.
set -euo pipefail

harness=$1
bin=$2
data=$3
logs=$4
shift 4
mkdir -p "$logs"
modules="image-source,obs-x264,obs-ffmpeg,obs-outputs,obs-dstreamy"
timeout=60
until_video=60

run() {
  local name=$1
  shift
  echo "=== $name"
  local rc=0
  "$harness" --seconds "$timeout" --until-video "$until_video" --output dstreamy_output \
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

if [[ ${OS:-} == Windows_NT ]]; then
  absent="/dstreamy-absent/d/s.sock"
  not_running="D-Streamy for OBS doesn't support Windows yet"
  not_running_line="^HARNESS expected_error code=-2: D-Streamy for OBS doesn't support Windows yet"
else
  absent="${TMPDIR:-/tmp}/dstreamy-absent-$$/d/s.sock"
  not_running="The D-Streamy service is not running"
  not_running_line="^HARNESS expected_error code=-2: The D-Streamy service is not running \(no service is listening at $absent\)"
fi

run not-running --no-encoders --seconds 3 --output-settings "{\"endpoint\": \"$absent\"}" --expect-error "$not_running" "$@"
expect not-running '\[obs-dstreamy\] loaded \(version '
expect not-running "$not_running_line"
expect not-running '^HARNESS alive after the stop: [1-9][0-9]* more frames rendered'
expect not-running 'HARNESS shutdown libobs_leaks=0'
expect not-running 'HARNESS result=PASS'

run start-panic --no-encoders --output-settings '{"panic_in": "start"}' --expect-panics 1 \
  --expect-error "panic caught in start: test panic in Start" "$@"
expect start-panic '\[obs-dstreamy\] panic caught in start: test panic in Start'
expect start-panic '^HARNESS expected_error start: D-Streamy plugin error \(panic caught in start'
expect start-panic 'HARNESS shutdown libobs_leaks=0'
expect start-panic 'HARNESS result=PASS'

settings_error() {
  local name=$1 error=$2
  shift 2
  run "$name" --seconds 3 --expect-error "$error" "$@"
  expect "$name" "^HARNESS expected_error start: .*$error"
  expect "$name" 'HARNESS shutdown libobs_leaks=0'
  expect "$name" 'HARNESS result=PASS'
}

share="--video-encoder obs_x264 --audio-encoder ffmpeg_opus"
no_fallback="{\"mode\": \"share\", \"share_fallback\": false, \"endpoint\": \"$absent\"}"
# shellcheck disable=SC2086
settings_error share-bf2 "D-Streamy needs B-frames off: " $share --bf 2 --keyint-sec 2 --output-settings "$no_fallback" "$@"
# shellcheck disable=SC2086
settings_error share-bf-unset "D-Streamy needs B-frames off: " $share --keyint-sec 2 --output-settings "$no_fallback" "$@"
# shellcheck disable=SC2086
settings_error share-keyint0 "D-Streamy needs a keyframe interval of 1 or 2 seconds, not 0" \
  $share --bf 0 --keyint-sec 0 --output-settings "$no_fallback" "$@"
# shellcheck disable=SC2086
settings_error share-1440p "D-Streamy needs an output size of at most 1920×1080 with even sides, not 2560×1440" \
  $share --bf 0 --keyint-sec 2 --width 2560 --height 1440 --output-settings "$no_fallback" "$@"
settings_error own-24fps "D-Streamy needs 15, 30 or 60 FPS, not 24: Settings › Video › Common FPS Values" \
  --no-encoders --fps 24 --output-settings "{\"endpoint\": \"$absent\"}" "$@"
modules="image-source,obs-x264,obs-dstreamy"
settings_error no-opus "D-Streamy needs the FFmpeg Opus audio encoder" \
  --no-encoders --output-settings "{\"endpoint\": \"$absent\"}" "$@"

echo "rust-plugin: PASS"
