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

echo "==> Building release..."
"$SCRIPT_DIR/build.sh"

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
    <string>0.1.0</string>
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

echo "==> Signing with: $IDENTITY"

# Sign daemon first (nested code)
if [ -f "$APP/Contents/Resources/daemon" ]; then
    codesign --force --sign "$IDENTITY" \
        --identifier "${BUNDLE_ID}.daemon" \
        "$APP/Contents/Resources/daemon"
fi

# Sign main app
codesign --force --sign "$IDENTITY" \
    --identifier "$BUNDLE_ID" \
    --entitlements "$ENTITLEMENTS" \
    --options runtime \
    "$APP"

rm "$ENTITLEMENTS"

# Verify
echo "==> Verifying signature..."
codesign --verify --deep --strict "$APP"
echo "==> Signature valid"

echo ""
echo "==> Release build: $APP"
echo "    Signed with: $IDENTITY"
echo "    Note: Works on your machine. Others need right-click → Open."
