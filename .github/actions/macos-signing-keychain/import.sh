#!/bin/bash
set -euo pipefail
umask 077
dir="$RUNNER_TEMP/signing-keychain"
keychain="$dir/signing.keychain-db"
p12="$dir/cert.p12"
mkdir -p "$dir"
trap 'rm -f "$p12"' EXIT

: "${CERT_P12:?cert-p12 is empty}" "${CERT_PASSWORD:?cert-password is empty}" "${GITHUB_ENV:?GITHUB_ENV is unset}"
if [[ $CERT_PASSWORD == *$'\n'* || $CERT_PASSWORD == *$'\r'* ]]; then
  echo "::error::DEVELOPER_ID_CERT_PASSWORD contains a line break (often a trailing newline from 'gh secret set < file'); set it again without one." >&2
  exit 1
fi
printf '%s' "$CERT_P12" | base64 --decode > "$p12"
password=$(openssl rand -hex 32)

quote() {
  local s=${1//\\/\\\\}
  printf '"%s"' "${s//\"/\\\"}"
}

# Commands carrying a password go through security's stdin so they never appear in the process list.
secure() {
  local name=$1 line=$2 err
  if ! err=$(printf '%s\n' "$line" | security -i 2>&1 >/dev/null) || grep -Eqi 'error|fail|usage' <<<"$err"; then
    echo "::error::security $name failed: $err" >&2
    exit 1
  fi
}

secure create-keychain "create-keychain -p $(quote "$password") $(quote "$keychain")"
security set-keychain-settings -lut 21600 "$keychain"
secure unlock-keychain "unlock-keychain -p $(quote "$password") $(quote "$keychain")"
secure import "import $(quote "$p12") -k $(quote "$keychain") -P $(quote "$CERT_PASSWORD") -T /usr/bin/codesign"
secure set-key-partition-list \
  "set-key-partition-list -S apple-tool:,apple:,codesign: -s -k $(quote "$password") $(quote "$keychain")"

if ! security find-identity -v -p codesigning "$keychain" | grep -Eq '^[[:space:]]*[1-9][0-9]* valid identit'; then
  echo "::error::No valid codesigning identity in the imported certificate; check DEVELOPER_ID_CERT_P12 and its password." >&2
  exit 1
fi

# Signing passes this keychain explicitly; the shared search list stays untouched so concurrent jobs can't drop each other's.
if ! security find-certificate -c "Developer ID Certification Authority" >/dev/null 2>&1; then
  echo "::error::The Developer ID intermediate certificate is not in the runner's keychains; codesign needs it to build the chain. Install Apple's Developer ID G2 intermediate in the System keychain." >&2
  exit 1
fi
echo "SIGNING_KEYCHAIN=$keychain" >> "$GITHUB_ENV"
