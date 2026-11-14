#!/usr/bin/env bash
# Usage: make-arch-repo.sh REPO_DIR [--sign PUBLIC_KEY]
# Adds REPO_DIR/<arch>/*.pkg.tar.zst to that architecture's d-streamy database. With --sign, signs every package and
# database with the secret key in PACMAN_SIGNING_KEY, which must be PUBLIC_KEY's, and copies PUBLIC_KEY to
# REPO_DIR/d-streamy.asc.
set -euo pipefail

repo=$1
sign=${2:-}
public_key=${3:-}
dirs=("$repo"/*/)
[ -d "${dirs[0]}" ] || { echo "$repo holds no architecture folders" >&2; exit 1; }

repo_add=()
if [ "$sign" = --sign ]; then
  [ -f "$public_key" ] || { echo "--sign needs the published public key file" >&2; exit 1; }
  [ -n "${PACMAN_SIGNING_KEY:-}" ] || { echo "PACMAN_SIGNING_KEY is not set; a published repository must be signed" >&2; exit 1; }
  GNUPGHOME=$(mktemp -d)
  export GNUPGHOME
  trap 'gpgconf --kill all >/dev/null 2>&1 || true; rm -rf "$GNUPGHOME"' EXIT
  printf '%s\n' "$PACMAN_SIGNING_KEY" | gpg --batch --quiet --import
  fingerprint=$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr:/ { print $10; exit }')
  [[ $fingerprint =~ ^[0-9A-F]{40,64}$ ]] || { echo "PACMAN_SIGNING_KEY holds no secret key" >&2; exit 1; }
  expected=$(gpg --batch --with-colons --show-keys "$public_key" | awk -F: '/^fpr:/ { print $10; exit }')
  if [ "$fingerprint" != "$expected" ]; then
    echo "PACMAN_SIGNING_KEY is key $fingerprint, but $public_key is ${expected:-no key}" >&2
    exit 1
  fi
  cp "$public_key" "$repo/d-streamy.asc"
  repo_add=(--sign --key "$fingerprint")
elif [ -n "$sign" ]; then
  echo "unknown option $sign" >&2
  exit 1
fi

for dir in "${dirs[@]}"; do
  packages=("$dir"*.pkg.tar.zst)
  [ -f "${packages[0]}" ] || { echo "$dir holds no packages" >&2; exit 1; }
  if [ "$sign" = --sign ]; then
    for package in "${packages[@]}"; do
      gpg --batch --yes --detach-sign --no-armor --local-user "$fingerprint" "$package"
    done
  fi
  repo-add "${repo_add[@]}" "${dir}d-streamy.db.tar.zst" "${packages[@]}"
done
