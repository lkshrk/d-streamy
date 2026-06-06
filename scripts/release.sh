#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."
# Find signing identity
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    IDENTITY="$CODESIGN_IDENTITY"
else
    IDENTITY=$(security find-identity -v -p codesigning | grep "Apple Development: dev@harke.me" | head -1 | sed 's/.*"\(.*\)".*/\1/')
fi

if [ -z "$IDENTITY" ]; then
    echo "No default identity found. Available identities:"
    echo ""
    mapfile -t CERTS < <(security find-identity -v -p codesigning | grep -v "valid identities" | sed 's/.*"\(.*\)".*/\1/')
    if [ ${#CERTS[@]} -eq 0 ]; then
        echo "  None found. Install a certificate first."
        exit 1
    fi
    for i in "${!CERTS[@]}"; do
        echo "  [$((i+1))] ${CERTS[$i]}"
    done
    echo ""
    read -rp "Select identity [1-${#CERTS[@]}]: " choice
    if [[ "$choice" -ge 1 && "$choice" -le ${#CERTS[@]} ]]; then
        IDENTITY="${CERTS[$((choice-1))]}"
    else
        echo "Invalid selection."
        exit 1
    fi
fi
BUNDLE_ID="me.harke.d-streamy"
APP_NAME="D-Streamy"
VERSION="${VERSION:-${TAG:-0.1.0}}"
VERSION="${VERSION#v}"

echo "==> Building release (version $VERSION)..."
SKIP_SIGN=1 "$SCRIPT_DIR/build.sh"

APP="$ROOT/bin/$APP_NAME.app"

# Generate Info.plist
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>D-Streamy needs screen recording access to capture your window for streaming.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>D-Streamy needs microphone access to capture audio for streaming.</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>D-Streamy needs system audio access to capture window audio for streaming.</string>
</dict>
</plist>
EOF

# Generate entitlements
ENTITLEMENTS=$(mktemp)
cat > "$ENTITLEMENTS" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <false/>
    <key>com.apple.security.network.client</key>
    <true/>
    <key>com.apple.security.device.audio-input</key>
    <true/>
</dict>
</plist>
EOF

# Bun runs JavaScriptCore (JIT) and dlopens native .node addons; under a
# hardened runtime it needs these entitlements or it crashes after notarization.
BUN_ENTITLEMENTS=$(mktemp)
cat > "$BUN_ENTITLEMENTS" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-jit</key>
    <true/>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
</dict>
</plist>
EOF

echo "==> Signing with: $IDENTITY"

# Timestamp requires network; notarization requires a secure timestamp. Allow
# local offline signing to skip it via NO_TIMESTAMP=1.
TS_FLAG="--timestamp"
[ "${NO_TIMESTAMP:-0}" = "1" ] && TS_FLAG="--timestamp=none"

# Sign nested Mach-O inside-out: the bundled bun runtime and every native
# addon/binary under Resources (.node, .dylib, ffmpeg, etc.) must be signed
# and hardened before the outer app, or notarization rejects them. The bun
# binary gets the JIT/library-validation entitlements above.
BUN_PATH="$APP/Contents/Resources/bun"
echo "==> Signing nested Mach-O..."
while IFS= read -r f; do
    if file "$f" | grep -q "Mach-O"; then
        if [ "$f" = "$BUN_PATH" ]; then
            codesign --force --options runtime $TS_FLAG \
                --entitlements "$BUN_ENTITLEMENTS" --sign "$IDENTITY" "$f"
        else
            codesign --force --options runtime $TS_FLAG --sign "$IDENTITY" "$f"
        fi
    fi
done < <(find "$APP/Contents/Resources" -type f)

# Sign main app last
codesign --force --sign "$IDENTITY" \
    --identifier "$BUNDLE_ID" \
    --entitlements "$ENTITLEMENTS" \
    --options runtime \
    $TS_FLAG \
    "$APP"

rm "$ENTITLEMENTS" "$BUN_ENTITLEMENTS"

# Verify
echo "==> Verifying signature..."
codesign --verify --deep --strict "$APP"
echo "==> Signature valid"

echo ""
echo "==> Release build: $APP"
echo "    Signed with: $IDENTITY"
echo "    Note: Works on your machine. Others need right-click → Open."
