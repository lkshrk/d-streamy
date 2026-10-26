#!/usr/bin/env bash
# Usage: apt-install.sh PACKAGE...
# Installs Ubuntu packages through $APT_ARCHIVES (default ~/apt-archives), a directory
# that actions/cache may have restored, and records its contents in $APT_ARCHIVES.manifest.
set -euo pipefail

cache=${APT_ARCHIVES:-$HOME/apt-archives}
apt=(-o "Dir::Cache::archives=$cache/" -o Acquire::Retries=3 -o Acquire::http::Timeout=30
  -o Acquire::https::Timeout=30 -o DPkg::Lock::Timeout=120)
args=(-y --no-install-recommends "$@")

retry() {
  for i in 1 2 3; do
    "$@" && return
    echo "attempt $i failed: $*"
    if [ "$i" -lt 3 ]; then sleep 15; fi
  done
  return 1
}

sudo mkdir -p "$cache/partial"
retry sudo apt-get "${apt[@]}" update

# apt trusts any archive of the right size in its cache; only the signed index's SHA256 makes a restored one safe.
for deb in "$cache"/*.deb; do
  [ -e "$deb" ] || continue
  IFS=_ read -r pkg ver arch <<<"$(basename "$deb" .deb)"
  want=$(apt-cache show "$pkg=${ver//%3a/:}" 2>/dev/null |
    awk -v a="$arch" '/^Architecture:/ { m = ($2 == a) } /^SHA256:/ && m { print $2; exit }')
  if [ -z "$want" ] || [ "$(sha256sum "$deb" | cut -d' ' -f1)" != "$want" ]; then
    sudo rm -f "$deb"
  fi
done
sudo find "$cache" -maxdepth 1 ! -type d ! -name '*.deb' ! -name lock -delete
sudo find "$cache" -type f ! -name lock -printf '%P %s\n' | sort >"$cache.manifest"

retry sudo apt-get "${apt[@]}" install --download-only "${args[@]}"
# Only fetches are retried: killing an install mid-unpack leaves dpkg broken.
sudo apt-get "${apt[@]}" install --no-download "${args[@]}"
