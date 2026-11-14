#!/usr/bin/env bash
# Usage: fill-pkgbuild.sh VERSION DIST_DIR PKGBUILD > filled
# Sets pkgver and pkgrel=1 and fills every sha256sums array from DIST_DIR's checksums of its sources; refuses placeholders.
set -euo pipefail

version=$1
dist=$2
pkgbuild=$3

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must be X.Y.Z, got '$version'" >&2; exit 1; }

filled=$(sed -E -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" "$pkgbuild")

sources() {
  bash -c 'source /dev/stdin; declare -n list=$1; [ "${#list[@]}" -gt 0 ] && printf "%s\n" "${list[@]}"' _ "$1" <<<"$filled"
}

keys=$(grep -oE '^sha256sums(_[a-z0-9_]+)?=' <<<"$filled" | tr -d '=') || true
[ -n "$keys" ] || { echo "$pkgbuild has no sha256sums array" >&2; exit 1; }

for key in $keys; do
  urls=$(sources "source${key#sha256sums}") || { echo "$pkgbuild has no source array for $key" >&2; exit 1; }
  sums=()
  while IFS= read -r url; do
    file="$dist/${url##*/}.sha256"
    [ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
    sum=$(cut -d' ' -f1 "$file")
    [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "$file holds no sha256" >&2; exit 1; }
    sums+=("'$sum'")
  done <<<"$urls"
  grep -Eq "^$key=\([^)]*\)$" <<<"$filled" || { echo "$key must be a one-line array" >&2; exit 1; }
  filled=$(sed -E "s/^$key=\([^)]*\)$/$key=(${sums[*]})/" <<<"$filled")
done

grep -qx "pkgver=$version" <<<"$filled" || { echo "pkgver line not filled" >&2; exit 1; }
if grep -Eq "(0{64}|1{64})" <<<"$filled"; then
  echo "a placeholder sha256 is left in $pkgbuild" >&2
  exit 1
fi
printf '%s\n' "$filled"
