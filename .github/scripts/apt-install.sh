#!/usr/bin/env bash
# Usage: APT_ARCHIVES=dir apt-install.sh PACKAGE...
set -euo pipefail

cache=${APT_ARCHIVES:-$HOME/apt-archives}
apt=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30
  -o DPkg::Lock::Timeout=120)
args=(-y --no-install-recommends "$@")

retry() {
  for i in 1 2 3; do
    "$@" && return
    echo "attempt $i failed: $*"
    if [ "$i" -lt 3 ]; then sleep 15; fi
  done
  return 1
}

retry sudo apt-get "${apt[@]}" update

# apt trusts any right-sized cached archive; only the signed index's SHA256 vouches for it.
empty=$(mktemp -d)
declare -A want=()
while read -r _ name _ sum; do
  if [[ $sum == SHA256:* ]]; then want[$name]=${sum#SHA256:}; fi
done < <(sudo apt-get "${apt[@]}" -o "Dir::Cache::archives=$empty/" -o Acquire::ForceHash=SHA256 \
  install --print-uris -qq "${args[@]}")
rmdir "$empty"

found=()
while IFS= read -r -d '' name; do
  if [ -n "${want[$name]-}" ]; then found+=("$cache/$name"); fi
done < <(sudo find "$cache" -mindepth 1 -maxdepth 1 -type f -links 1 -printf '%f\0' 2>/dev/null)

good=()
if [ ${#found[@]} -gt 0 ]; then
  while read -r sum path; do
    if [ "$sum" = "${want[${path##*/}]}" ]; then good+=("$path"); fi
  done < <(sudo sha256sum -- "${found[@]}")
fi

# Rebuilding the dir drops everything unverified, whatever its type, mode or name.
fresh=$(sudo mktemp -d "$cache.XXXXXX")
if [ ${#good[@]} -gt 0 ]; then sudo mv -t "$fresh" -- "${good[@]}"; fi
sudo rm -rf "$cache"
sudo mv -T "$fresh" "$cache"
sudo mkdir "$cache/partial"
sudo chown -R root:root "$cache"
sudo chmod -R u=rwX,go=rX "$cache"

sudo rm -rf "$cache.manifest"
sudo find "$cache" -type f ! -name lock ! -path '*/partial/*' -printf '%P %s\n' | sort >"$cache.manifest"

apt+=(-o "Dir::Cache::archives=$cache/")
retry sudo apt-get "${apt[@]}" install --download-only "${args[@]}"
# Only fetches are retried: killing an install mid-unpack leaves dpkg broken.
sudo apt-get "${apt[@]}" install --no-download "${args[@]}"
