#!/usr/bin/env bash
# usage: frontend-smoke.sh <obs-config-dir> <log-file> <obs-binary> [obs args...]
set -uo pipefail

config_dir=$1
log_file=$2
shift 2

mkdir -p "$config_dir/plugin_config/obs-websocket"
# LastVersion is LIBOBS_API_VER for 32.2.2; with FirstRun set it suppresses the auto-config wizard (frontend/widgets/OBSBasic.cpp:1286).
cat >"$config_dir/global.ini" <<'EOF'
[General]
LastVersion=537001986
Pre31Migrated=true
MacOSPermissionsDialogLastShown=1
EnableAutoUpdates=false
InfoIncrement=99999
EOF
cat >"$config_dir/user.ini" <<'EOF'
[General]
FirstRun=true
Pre19Defaults=false
Pre21Defaults=false
Pre23Defaults=false
Pre24.1Defaults=false
EOF
cat >"$config_dir/plugin_config/obs-websocket/config.json" <<'EOF'
{"first_load": false, "server_enabled": true, "server_port": 4455, "alerts_enabled": false, "auth_required": false, "server_password": ""}
EOF

"$@" --multi --disable-updater --verbose --websocket_port 4455 >"$log_file" 2>&1 &
obs_pid=$!
echo "started obs pid=$obs_pid"

python_bin=$(command -v python3 || command -v python)
"$python_bin" "$(dirname "$0")/ws_hello.py" 4455 120
ws_status=$?

if kill -0 "$obs_pid" 2>/dev/null; then
  echo "obs still running after websocket check"
  kill "$obs_pid" 2>/dev/null
  sleep 5
  kill -9 "$obs_pid" 2>/dev/null
else
  wait "$obs_pid"
  echo "obs exited early with status $?"
fi

echo "--- obs stdout/stderr (tail) ---"
tail -n 80 "$log_file"
latest_log=$(ls -t "$config_dir"/logs/*.txt 2>/dev/null | head -n 1)
if [[ -n $latest_log ]]; then
  echo "--- obs log $latest_log (tail) ---"
  tail -n 120 "$latest_log"
  cp "$latest_log" "$(dirname "$log_file")/obs-frontend-internal.log"
fi

if [[ $ws_status -eq 0 ]]; then
  echo "FRONTEND RESULT=PASS"
else
  echo "FRONTEND RESULT=FAIL"
fi
exit "$ws_status"
