#!/usr/bin/env bash
# Usage: fill-formula.sh VERSION DIST_DIR FORMULA > filled.rb
# Fills version and the two macOS sha256 lines of the Homebrew formula from the
# release checksums in DIST_DIR and refuses to print a formula with placeholders.
set -euo pipefail

version=$1
dist=$2
formula=$3

sum_of() {
  local file="$dist/dstreamy-$version-$1.tar.gz.sha256" sum
  sum=$(cut -d' ' -f1 "$file")
  [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "$file holds no sha256" >&2; exit 1; }
  printf '%s\n' "$sum"
}

arm=$(sum_of aarch64-apple-darwin)
x86=$(sum_of x86_64-apple-darwin)

filled=$(awk -v v="$version" -v arm="$arm" -v x86="$x86" '
  /^  version "/ { sub(/"[^"]*"/, "\"" v "\"") }
  /aarch64-apple-darwin/ { h = arm }
  /x86_64-apple-darwin/ { h = x86 }
  /sha256 "/ && h != "" { sub(/"[^"]*"/, "\"" h "\""); h = "" }
  { print }
' "$formula")

grep -Fq "version \"$version\"" <<<"$filled" || { echo "version line not filled" >&2; exit 1; }
grep -Fq "sha256 \"$arm\"" <<<"$filled" || { echo "arm64 sha256 not filled" >&2; exit 1; }
grep -Fq "sha256 \"$x86\"" <<<"$filled" || { echo "x86_64 sha256 not filled" >&2; exit 1; }
if grep -Eq 'sha256 "(0{64}|1{64})"' <<<"$filled"; then
  echo "a placeholder sha256 is left in the formula" >&2
  exit 1
fi
printf '%s\n' "$filled"
