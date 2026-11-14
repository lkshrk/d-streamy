#!/usr/bin/env bash
# Usage: check-release-assets.sh VERSION DIST_DIR
# Verifies every checksum and that each asset a release needs is present with its expected contents.
set -euo pipefail

version=$1
dist=$2
cd "$dist"
ls -l
sha256sum -c -- *.sha256

need() { test -f "$1" || { echo "missing $1" >&2; exit 1; }; test -f "$1.sha256" || { echo "missing $1.sha256" >&2; exit 1; }; }

for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  archive="dstreamy-$version-$target.tar.gz"
  need "$archive"
  for entry in dstreamy README.md LICENSE THIRD_PARTY_NOTICES.txt; do
    tar -tzf "$archive" | grep -x "dstreamy-$version-$target/$entry" >/dev/null \
      || { echo "$archive lacks $entry" >&2; exit 1; }
  done
done

windows="dstreamy-$version-x86_64-pc-windows-msvc.zip"
need "$windows"
for entry in dstreamy.exe README.md LICENSE THIRD_PARTY_NOTICES.txt; do
  unzip -Z1 "$windows" | grep -x "dstreamy-$version-x86_64-pc-windows-msvc/$entry" >/dev/null \
    || { echo "$windows lacks $entry" >&2; exit 1; }
done

app="D-Streamy-$version-macos-universal.zip"
need "$app"
unzip -l "$app" | grep ' D-Streamy.app/Contents/MacOS/dstreamy$' >/dev/null || { echo "$app lacks the bundled dstreamy" >&2; exit 1; }

need "obs-dstreamy-$version-linux-x86_64.tar.gz"
tar -tzf "obs-dstreamy-$version-linux-x86_64.tar.gz" | grep 'obs-plugins/obs-dstreamy.so$' >/dev/null \
  || { echo "the Linux plugin archive lacks obs-dstreamy.so" >&2; exit 1; }
need "obs-dstreamy-$version-macos-universal.zip"
plugin="obs-dstreamy-$version-windows-x64.zip"
need "$plugin"
for entry in obs-dstreamy.dll dstreamy.exe dstreamy-ui/obs-dstreamy-ui.dll dstreamy-ui/THIRD_PARTY_NOTICES.txt; do
  unzip -Z1 "$plugin" | grep -x "obs-dstreamy/bin/64bit/$entry" >/dev/null \
    || { echo "$plugin lacks $entry" >&2; exit 1; }
done
