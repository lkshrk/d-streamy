#!/usr/bin/env bash
# Usage: check-arch-live.sh VERSION BASE_URL
# Refuses a deploy when BASE_URL/<arch>/d-streamy.db already serves a dstreamy-bin newer than VERSION-1.
set -euo pipefail

version=${1:?usage: check-arch-live.sh VERSION BASE_URL}
base=${2:?usage: check-arch-live.sh VERSION BASE_URL}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for arch in x86_64 aarch64; do
  url="$base/$arch/d-streamy.db"
  status=$(curl -sS --retry 3 --connect-timeout 20 --max-time 120 -o "$tmp/$arch.db" -w '%{http_code}' "$url") || { echo "cannot fetch $url" >&2; exit 1; }
  case $status in
    404) echo "$arch: no live repository yet"; continue ;;
    200) ;;
    *) echo "$url answered HTTP $status" >&2; exit 1 ;;
  esac
  bsdtar -xOf "$tmp/$arch.db" '*/desc' > "$tmp/$arch.desc"
  live=$(awk '/^%NAME%$/ { getline; name = $0 }
    /^%VERSION%$/ { getline; if (name == "dstreamy-bin") { print; exit } }' "$tmp/$arch.desc")
  [ -n "$live" ] || { echo "$url lists no dstreamy-bin" >&2; exit 1; }
  if [ "$(vercmp "$live" "$version-1")" -gt 0 ]; then
    echo "$url already serves dstreamy-bin $live, newer than $version-1; refusing to replace it" >&2
    exit 1
  fi
  echo "$arch: live $live, deploying $version-1"
done
