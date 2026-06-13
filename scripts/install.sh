#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."
APP_NAME="D-Streamy"
SRC="$ROOT/bin/$APP_NAME.app"
DEST="/Applications/$APP_NAME.app"

# Local installs use the dev build (build.sh): fast, dev-signed with a stable
# identity so macOS permissions persist across rebuilds. Pass --release to use
# the notarized pipeline (release.sh) instead.
BUILD_SCRIPT="$SCRIPT_DIR/build.sh"
if [ "${1:-}" = "--release" ]; then
    BUILD_SCRIPT="$SCRIPT_DIR/release.sh"
fi

# Build if missing or source is newer
NEEDS_BUILD=false
if [ ! -d "$SRC" ]; then
    NEEDS_BUILD=true
elif [ -n "$(find "$ROOT/App" "$ROOT/src" "$ROOT/capture" -newer "$SRC" -type f 2>/dev/null | head -1)" ]; then
    NEEDS_BUILD=true
fi

if [ "$NEEDS_BUILD" = true ]; then
    echo "==> Building via $(basename "$BUILD_SCRIPT") (source newer than build or no build found)..."
    "$BUILD_SCRIPT"
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
