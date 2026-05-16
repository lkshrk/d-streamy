#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."
APP_NAME="D-Streamy"
SRC="$ROOT/bin/$APP_NAME.app"
DEST="/Applications/$APP_NAME.app"

# Build if missing or source is newer
NEEDS_BUILD=false
if [ ! -d "$SRC" ]; then
    NEEDS_BUILD=true
elif [ -n "$(find "$ROOT/App" "$ROOT/src" "$ROOT/capture" -newer "$SRC" -type f 2>/dev/null | head -1)" ]; then
    NEEDS_BUILD=true
fi

if [ "$NEEDS_BUILD" = true ]; then
    echo "==> Building (source newer than build or no build found)..."
    "$SCRIPT_DIR/release.sh"
fi

# Kill running instance
pkill -f "$APP_NAME" 2>/dev/null || true

# Remove old install
if [ -d "$DEST" ]; then
    rm -rf "$DEST"
fi

# Copy to Applications
cp -R "$SRC" "$DEST"

# Remove quarantine (skip Gatekeeper warning)
xattr -cr "$DEST"

echo "==> Installed: $DEST"
echo "    Quarantine cleared — no Gatekeeper prompt."
echo ""
echo "    Launch: open /Applications/$APP_NAME.app"
