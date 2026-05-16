#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

echo "==> Building CaptureLib + App (Swift)..."
swift build -c release --package-path "$ROOT"

echo "==> Building daemon (Bun)..."
cd "$ROOT"
rm -rf .build/daemon-bundle
bun build src/daemon/index.ts --outdir .build/daemon-bundle --target node \
    --external ffmpeg-static \
    --external ws \
    --external @snazzah/davey-darwin-arm64 \
    --external @seydx/node-av-darwin-arm64 \
    --external @lng2004/node-datachannel \
    --external @img/sharp-darwin-arm64 \
    --external @img/sharp-libvips-darwin-arm64

echo "==> Assembling D-Streamy.app..."
APP="$ROOT/bin/D-Streamy.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Copy app binary
cp "$(swift build -c release --package-path "$ROOT" --show-bin-path)/D-Streamy" "$APP/Contents/MacOS/D-Streamy"

# Copy daemon bundle (JS + native .node addons)
rm -rf "$APP/Contents/Resources/daemon"
cp -R "$ROOT/.build/daemon-bundle/" "$APP/Contents/Resources/daemon/"

# Copy native addons that can't be bundled
copy_native_pkg() {
    local pkg_path="$1"
    if [ -d "$ROOT/node_modules/$pkg_path" ]; then
        mkdir -p "$APP/Contents/Resources/daemon/node_modules/$pkg_path"
        cp -R "$ROOT/node_modules/$pkg_path/" "$APP/Contents/Resources/daemon/node_modules/$pkg_path/"
    fi
}
copy_native_pkg "@snazzah/davey-darwin-arm64"
copy_native_pkg "@seydx/node-av-darwin-arm64"
copy_native_pkg "@lng2004/node-datachannel"
copy_native_pkg "@img/sharp-darwin-arm64"
copy_native_pkg "@img/sharp-libvips-darwin-arm64"
copy_native_pkg "ws"


# Compile and copy app icon
echo "==> Compiling asset catalog..."
xcrun actool "$ROOT/App/Assets.xcassets" \
    --compile "$APP/Contents/Resources" \
    --platform macosx \
    --minimum-deployment-target 14.0 \
    --app-icon AppIcon \
    --output-partial-info-plist /dev/null > /dev/null

# Codesign (stable identity so macOS permissions persist across rebuilds)
codesign --force --sign "Apple Development: dev@harke.me (L2GZ9Q95QW)" --identifier me.harke.d-streamy "$APP"

echo "==> Done: $APP"
