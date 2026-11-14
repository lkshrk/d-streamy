#!/usr/bin/env bash
# Usage: fill-cask.sh VERSION DIST_DIR CASK > filled.rb
# Fills version and sha256 of the Homebrew cask from the app's release checksum and refuses a placeholder.
set -euo pipefail

version=$1
dist=$2
cask=$3

file="$dist/D-Streamy-$version-macos-universal.zip.sha256"
sum=$(cut -d' ' -f1 "$file")
[[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "$file holds no sha256" >&2; exit 1; }

filled=$(sed -E -e "s/^  version \"[^\"]*\"/  version \"$version\"/" -e "s/^  sha256 \"[^\"]*\"/  sha256 \"$sum\"/" "$cask")
grep -Fq "version \"$version\"" <<<"$filled" || { echo "version line not filled" >&2; exit 1; }
grep -Fq "sha256 \"$sum\"" <<<"$filled" || { echo "sha256 not filled" >&2; exit 1; }
printf '%s\n' "$filled"
