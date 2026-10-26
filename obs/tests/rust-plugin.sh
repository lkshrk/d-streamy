#!/usr/bin/env bash
# usage: rust-plugin.sh <obs-harness> <plugin-bin-path> <plugin-data-path> <log-dir> [harness args...] (macOS paths: %module% patterns)
# Runs without a D-Streamy service; streaming through `dstreamy serve` is rust/crates/dstreamy-cli/tests/obs_plugin.rs.
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
  "$harness" --seconds "$timeout" --until-video "$until_video" --output dstreamy_output --video-encoder obs_x264 --audio-encoder ffmpeg_opus \
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

run not-running --seconds 3 --output-settings "{\"endpoint\": \"$absent\"}" --expect-error "$not_running" "$@"
expect not-running '\[obs-dstreamy\] loaded \(version '
expect not-running "$not_running_line"
expect not-running '^HARNESS alive after the stop: [1-9][0-9]* more frames rendered'
expect not-running 'HARNESS shutdown libobs_leaks=0'
expect not-running 'HARNESS result=PASS'

run start-panic --output-settings '{"panic_in": "start"}' --expect-panics 1 \
  --expect-error "panic caught in start: test panic in Start" "$@"
expect start-panic '\[obs-dstreamy\] panic caught in start: test panic in Start'
expect start-panic '^HARNESS expected_error start: D-Streamy plugin error \(panic caught in start'
expect start-panic 'HARNESS shutdown libobs_leaks=0'
expect start-panic 'HARNESS result=PASS'

echo "rust-plugin: PASS"
