#!/usr/bin/env bash
# Usage: check-arch-repo.sh VERSION REPO_DIR [PUBLIC_KEY]
# Syncs each architecture's d-streamy database with a throwaway pacman setup, as root, and checks what it lists.
# With PUBLIC_KEY, pacman requires and verifies the signatures of the databases and the packages.
set -euo pipefail

usage="usage: check-arch-repo.sh VERSION REPO_DIR [PUBLIC_KEY]"
version=${1:?$usage}
repo=$(realpath -e "${2:?$usage}")
key=${3:-}
[ -z "$key" ] || [ -f "$key" ] || { echo "public key $key does not exist" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'gpgconf --homedir "$tmp/gpg" --kill all >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

mkdir -m 700 "$tmp/gpg"
siglevel=Never
if [ -n "$key" ]; then
  pacman-key --gpgdir "$tmp/gpg" --init >/dev/null 2>&1
  pacman-key --gpgdir "$tmp/gpg" --add "$key" >/dev/null
  fingerprint=$(gpg --homedir "$tmp/gpg" --with-colons --show-keys "$key" | awk -F: '/^fpr:/ { print $10; exit }')
  pacman-key --gpgdir "$tmp/gpg" --lsign-key "$fingerprint" >/dev/null
  siglevel=Required
fi

for arch in x86_64 aarch64; do
  case $arch in
    x86_64) expected=("dstreamy:dstreamy-bin" "obs-dstreamy:obs-dstreamy-bin") ;;
    aarch64) expected=("dstreamy:dstreamy-bin") ;;
  esac
  mkdir -p "$tmp/$arch/db" "$tmp/$arch/cache"
  conf="$tmp/$arch/pacman.conf"
  printf '[options]\nArchitecture = %s\nSigLevel = %s\n\n[d-streamy]\nServer = file://%s/%s\n' \
    "$arch" "$siglevel" "$repo" "$arch" > "$conf"
  pacman=(pacman --config "$conf" --dbpath "$tmp/$arch/db" --cachedir "$tmp/$arch/cache" --gpgdir "$tmp/gpg"
    --logfile "$tmp/$arch/pacman.log" --noconfirm)
  "${pacman[@]}" -Sy >/dev/null
  listed=$("${pacman[@]}" -Sl d-streamy | sort)
  want=$(for pair in "${expected[@]}"; do echo "d-streamy ${pair#*:} $version-1"; done | sort)
  [ "$listed" = "$want" ] || { printf '%s lists:\n%s\nexpected:\n%s\n' "$arch" "$listed" "$want" >&2; exit 1; }
  for pair in "${expected[@]}"; do
    resolved=$("${pacman[@]}" -Sddp --print-format '%n %v' "${pair%%:*}")
    [ "$resolved" = "${pair#*:} $version-1" ] || { echo "$arch: pacman -S ${pair%%:*} picks '$resolved'" >&2; exit 1; }
  done
  "${pacman[@]}" -Swdd "${expected[@]#*:}" >/dev/null
  echo "$arch: $(tr '\n' ' ' <<<"$listed")"
done
