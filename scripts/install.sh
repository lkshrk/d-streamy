#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."
APP_NAME="D-Streamy"
SRC="$ROOT/bin/$APP_NAME.app"
INSTALL_DIR="${DSTREAMY_INSTALL_DIR:-/Applications}"
INSTALL_DIR="$(cd "$INSTALL_DIR" 2>/dev/null && pwd)" || {
    echo "error: install directory ${DSTREAMY_INSTALL_DIR:-/Applications} does not exist" >&2
    exit 1
}
DEST="$INSTALL_DIR/$APP_NAME.app"

# Dev build by default (stable dev signature keeps permissions); --release uses release.sh.
BUILD_SCRIPT="$SCRIPT_DIR/build.sh"
if [ "${1:-}" = "--release" ]; then
    BUILD_SCRIPT="$SCRIPT_DIR/release.sh"
fi

echo "==> Building via $(basename "$BUILD_SCRIPT")..."
"$BUILD_SCRIPT"

# Stop the installed app and its bundled daemon, matched by executable path.
DEST_PATTERN=$(printf '%s' "$DEST" | sed 's/[][\.*^$+?(){}|]/\\&/g')
pkill -f "^$DEST_PATTERN/Contents/MacOS/" 2>/dev/null || true

rm -rf "$DEST"
cp -R "$SRC" "$DEST"
xattr -cr "$DEST"

echo "==> Installed: $DEST"
echo "    Quarantine cleared — no Gatekeeper prompt."
echo ""
echo "    Launch: open \"$DEST\""
