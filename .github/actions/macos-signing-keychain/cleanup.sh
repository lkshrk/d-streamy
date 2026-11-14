#!/bin/bash
set -uo pipefail
dir="$RUNNER_TEMP/signing-keychain"
keychain="$dir/signing.keychain-db"
[ -d "$dir" ] || exit 0

status=0
# delete-keychain would rewrite the shared search list; the keychain was never on it.
if [ -e "$keychain" ] && ! security lock-keychain "$keychain"; then
  echo "::warning::Could not lock $keychain before deleting it." >&2
  status=1
fi
rm -rf "$dir" || status=1
exit "$status"
