#!/usr/bin/env bash
# A frontend-checked plugin build must fail when obs-frontend-api.h is missing, never silently skip its ABI check.
# usage: obs-frontend-abi-check.sh <obs-config.txt from the obs/ build> <scratch dir>
set -euo pipefail

config=$1
scratch=$2
crate=$(cd "$(dirname "$0")/../../rust/crates/dstreamy-obs" && pwd)
rm -rf "$scratch"
mkdir -p "$scratch/include"

grep -qx 'define=DSTREAMY_FRONTEND_ABI_CHECK' "$config" ||
  { echo "FAIL: $config does not ask for the frontend ABI check"; exit 1; }

copies=0
while IFS= read -r line; do
  dir=${line#include=}
  if [[ $line == include=* && -f $dir/obs-frontend-api.h ]]; then
    copies=$((copies + 1))
    cp -R "$dir" "$scratch/include/$copies"
    rm "$scratch/include/$copies/obs-frontend-api.h"
    echo "include=$scratch/include/$copies"
  else
    echo "$line"
  fi
done <"$config" >"$scratch/obs-config.txt"
[[ $copies -gt 0 ]] || { echo "FAIL: no include folder in $config holds obs-frontend-api.h"; exit 1; }

cd "$crate"
if DSTREAMY_OBS_CONFIG="$scratch/obs-config.txt" cargo check --locked -p dstreamy-obs-sys \
  --target-dir "$scratch/target" >"$scratch/check.log" 2>&1; then
  cat "$scratch/check.log"
  echo "FAIL: the frontend-checked build passed without obs-frontend-api.h"
  exit 1
fi
if ! grep -q "DSTREAMY_FRONTEND_ABI_CHECK is set but obs-frontend-api.h is not on the include path" "$scratch/check.log"; then
  cat "$scratch/check.log"
  echo "FAIL: the build failed, but not on the missing frontend header"
  exit 1
fi
echo "PASS: without obs-frontend-api.h the frontend-checked build fails"
