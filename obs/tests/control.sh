#!/usr/bin/env bash
# usage: control.sh <obs-harness> <plugin-bin-path> <plugin-data-path> <log-dir>
# Drives the D-Streamy procs through obs-harness --control without a service; start and stop are not wired yet.
set -euo pipefail

harness=$1
bin=$2
data=$3
logs=$4
rm -rf "$logs"
mkdir -p "$logs"
runtime="$logs/runtime"
mkdir -m 700 "$runtime"

run() {
  local name=$1 config=$2
  echo "=== $name"
  local rc=0
  XDG_RUNTIME_DIR="$runtime" "$harness" --plugin-bin "$bin" --plugin-data "$data" --modules obs-dstreamy \
    --module-config "$config" --control "$logs/$name.json" >"$logs/$name.log" 2>&1 || rc=$?
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

cat >"$logs/basic.json" <<'EOF'
{"steps": [
  {"call": "dstreamy_get_settings", "request_id": 1},
  {"wait": "get_settings", "request_id": 1, "ok": true, "contains": "\"state\":{\"state\":\"idle\"}"},
  {"call": "dstreamy_get_settings", "request_id": 2},
  {"sleep_ms": 300},
  {"wait": "get_settings", "request_id": 2, "ok": true},
  {"call": "dstreamy_set_settings", "request_id": 3, "json": "{\"guild_id\": \"g\", \"bitrate_kbps\": 9000}"},
  {"wait": "set_settings", "request_id": 3, "ok": true, "contains": "\"bitrate_kbps\":9000"},
  {"config_contains": "9000"},
  {"call": "dstreamy_set_settings", "request_id": 4, "json": "{\"bitrate_kbps\": \"fast\"}"},
  {"wait": "set_settings", "request_id": 4, "ok": false, "contains": "bitrate_kbps must be"},
  {"call": "dstreamy_status", "request_id": 5},
  {"wait": "status", "request_id": 5, "ok": false, "contains": "\"service\":null"},
  {"call": "dstreamy_start", "request_id": 6},
  {"wait": "start", "request_id": 6, "ok": false, "contains": "not available in this build"},
  {"call": "dstreamy_channels", "request_id": 7, "guild_id": "", "accepted": false, "error": "guild_id"},
  {"call": "dstreamy_status", "request_id": 0, "accepted": false, "error": "request_id"},
  {"call": "dstreamy_stream_starting", "generation": 1, "output": true, "classification": "enhanced"},
  {"wait": "stream_starting", "request_id": 0, "contains": "\"generation\":1"},
  {"call": "dstreamy_stream_started", "generation": 1, "output": true, "classification": "bogus", "accepted": false, "error": "classification"},
  {"call": "dstreamy_shutdown", "request_id": 8},
  {"wait": "shutdown", "request_id": 8, "ok": true},
  {"call": "dstreamy_get_settings", "request_id": 9, "accepted": false, "error": "D-Streamy is shutting down"}
]}
EOF
run basic "$logs/basic-config"
expect basic '\[obs-dstreamy\] loaded \(version '
expect basic 'HARNESS shutdown libobs_leaks=0'
expect basic 'HARNESS result=PASS'

recovery="$logs/recovery-config/obs-dstreamy"
mkdir -p "$recovery"
printf '{"version": 1, "owned": "{\\"connections' >"$recovery/config.json"
owned='{\"connections\":[{\"id\":3,\"fps\":30}],\"guild_id\":\"g\",\"channel_id\":\"c\",\"instance\":{\"dev\":1,\"ino\":2,\"mtime\":3,\"mtime_nsec\":4}}'
printf '{"version": 1, "guild_id": "g", "channel_id": "c", "owned": "%s", "ownership_certainty": "pending"}' "$owned" \
  >"$recovery/config.json.bak"
cp "$recovery/config.json" "$logs/primary.before"
cp "$recovery/config.json.bak" "$logs/backup.before"
cat >"$logs/recovery.json" <<'EOF'
{"steps": [
  {"call": "dstreamy_get_settings", "request_id": 1},
  {"wait": "get_settings", "request_id": 1, "ok": true, "contains": "until"},
  {"call": "dstreamy_set_settings", "request_id": 2, "json": "{\"bitrate_kbps\": 7000}"},
  {"wait": "set_settings", "request_id": 2, "ok": false, "contains": "\"id\":3"},
  {"call": "dstreamy_start", "request_id": 3},
  {"wait": "start", "request_id": 3, "ok": false}
]}
EOF
run recovery "$logs/recovery-config"
expect recovery 'HARNESS result=PASS'
cmp "$recovery/config.json" "$logs/primary.before"
cmp "$recovery/config.json.bak" "$logs/backup.before"
echo "PASS control"
