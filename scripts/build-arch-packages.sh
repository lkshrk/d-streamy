#!/usr/bin/env bash
# Usage: build-arch-packages.sh VERSION DIST_DIR OUT_DIR
# Fills every packaging/arch PKGBUILD from DIST_DIR's release assets and builds it with makepkg for each of its
# architectures into OUT_DIR/<arch>/. Runs as a non-root user; makepkg refuses root.
set -euo pipefail

version=$1
dist=$(realpath "$2")
out=$(realpath -m "$3")
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

contains() {
  local pkg=$1 listing
  listing=$(bsdtar -tf "$pkg")
  for entry in "${@:2}"; do
    grep -qx "$entry" <<<"$listing" || { echo "${pkg##*/} lacks $entry" >&2; exit 1; }
  done
}

elf_machine() {
  local pkg=$1 file=$2 dir
  dir=$(mktemp -d -p "$work")
  bsdtar -xf "$pkg" -C "$dir" "$file"
  od -An -tx1 -j18 -N2 "$dir/$file" | tr -d ' \n'
}

licenses_match() {
  local notices=$1 dir found declared
  shift
  dir=$(mktemp -d -p "$work")
  for pkg in "$@"; do
    mkdir "$dir/${pkg##*/}"
    bsdtar -xf "$pkg" -C "$dir/${pkg##*/}" .PKGINFO "$notices"
  done
  found=$("$root/scripts/notice-licenses.sh" "$dir"/*/"$notices")
  declared=$(sed -n 's/^license = //p' "$dir"/*/.PKGINFO | grep -vx GPL-3.0-or-later | sort -u)
  if [ "$found" != "$declared" ]; then
    echo "the license array of ${1##*/} differs from the crates in its notices (< array, > notices):" >&2
    diff <(printf '%s\n' "$declared") <(printf '%s\n' "$found") >&2 || true
    exit 1
  fi
}

for dir in "$root"/packaging/arch/*/; do
  name=$(basename "$dir")
  mkdir -p "$work/$name/template"
  cp "$dir/PKGBUILD" "$work/$name/template/"
  if ! (cd "$work/$name/template" && makepkg --printsrcinfo) | diff -u "$dir.SRCINFO" -; then
    echo "packaging/arch/$name/.SRCINFO is stale; regenerate it with makepkg --printsrcinfo > .SRCINFO" >&2
    exit 1
  fi
  "$root/scripts/fill-pkgbuild.sh" "$version" "$dist" "$dir/PKGBUILD" > "$work/$name/PKGBUILD"
  ln -s "$dist"/*.tar.gz "$work/$name/"
  for arch in $(cd "$work/$name" && bash -c 'source PKGBUILD; printf "%s\n" "${arch[@]}"'); do
    mkdir -p "$out/$arch"
    (cd "$work/$name" && CARCH=$arch PKGDEST="$out/$arch" \
      makepkg --nodeps --noconfirm --noprogressbar --nocolor --cleanbuild --force)
  done
done

expect=(
  "x86_64/dstreamy-bin-$version-1-x86_64.pkg.tar.zst"
  "aarch64/dstreamy-bin-$version-1-aarch64.pkg.tar.zst"
  "x86_64/obs-dstreamy-bin-$version-1-x86_64.pkg.tar.zst"
)
built=$(cd "$out" && find . -name '*.pkg.tar.zst' -printf '%P\n' | sort)
[ "$built" = "$(printf '%s\n' "${expect[@]}" | sort)" ] || { printf 'built packages differ:\n%s\n' "$built" >&2; exit 1; }

for arch in x86_64 aarch64; do
  pkg="$out/$arch/dstreamy-bin-$version-1-$arch.pkg.tar.zst"
  contains "$pkg" usr/bin/dstreamy usr/share/licenses/dstreamy-bin/LICENSE \
    usr/share/licenses/dstreamy-bin/THIRD_PARTY_NOTICES.txt
  machine=$(elf_machine "$pkg" usr/bin/dstreamy)
  case $arch:$machine in
    x86_64:3e00 | aarch64:b700) ;;
    *) echo "${pkg##*/} holds a dstreamy for ELF machine $machine" >&2; exit 1 ;;
  esac
done
licenses_match usr/share/licenses/dstreamy-bin/THIRD_PARTY_NOTICES.txt "$out"/*/dstreamy-bin-*.pkg.tar.zst
plugin="$out/x86_64/obs-dstreamy-bin-$version-1-x86_64.pkg.tar.zst"
licenses_match usr/lib/obs-plugins/dstreamy-ui/THIRD_PARTY_NOTICES.txt "$plugin"
contains "$plugin" usr/lib/obs-plugins/obs-dstreamy.so usr/lib/obs-plugins/dstreamy-ui/obs-dstreamy-ui.so \
  usr/lib/obs-plugins/dstreamy-ui/THIRD_PARTY_NOTICES.txt usr/share/licenses/obs-dstreamy-bin/LICENSE
if bsdtar -tf "$plugin" | grep -E '^usr/lib/obs-plugins/[^/]+\.so$' | grep -vx usr/lib/obs-plugins/obs-dstreamy.so; then
  echo "libobs would try these as modules: only obs-dstreamy.so may sit in the module folder" >&2
  exit 1
fi
