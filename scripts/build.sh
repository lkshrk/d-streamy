#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

SWIFT_FLAGS=(-c release --package-path "$ROOT")
if [ "${UNIVERSAL:-0}" = "1" ]; then
    SWIFT_FLAGS+=(--arch arm64 --arch x86_64)
fi

echo "==> Building App (Swift)..."
swift build "${SWIFT_FLAGS[@]}"

# DSTREAMY_DAEMON points at a prebuilt (e.g. universal) dstreamy; otherwise build one for this Mac.
if [ -z "${DSTREAMY_DAEMON:-}" ]; then
    echo "==> Building dstreamy (Rust)..."
    (cd "$ROOT/rust" && cargo build --release --locked -p dstreamy-cli)
    DSTREAMY_DAEMON="$ROOT/rust/target/release/dstreamy"
fi

echo "==> Assembling D-Streamy.app..."
APP="$ROOT/bin/D-Streamy.app"
rm -rf "$APP/Contents/Resources/daemon" "$APP/Contents/Resources/bun"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$(swift build "${SWIFT_FLAGS[@]}" --show-bin-path)/D-Streamy" "$APP/Contents/MacOS/D-Streamy"
cp "$DSTREAMY_DAEMON" "$APP/Contents/MacOS/dstreamy"
chmod +x "$APP/Contents/MacOS/dstreamy"

if [ "${UNIVERSAL:-0}" = "1" ]; then
    NOTICE_TARGETS=(--target aarch64-apple-darwin --target x86_64-apple-darwin)
else
    NOTICE_TARGETS=(--target "$(rustc -vV | sed -n 's/^host: //p')")
fi
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
python3 "$SCRIPT_DIR/third-party-notices.py" --manifest "$ROOT/rust/Cargo.toml" \
    "${NOTICE_TARGETS[@]}" --output "$APP/Contents/Resources/THIRD_PARTY_NOTICES.txt"

# Compile and copy app icon
echo "==> Compiling asset catalog..."
xcrun actool "$ROOT/App/Assets.xcassets" \
    --compile "$APP/Contents/Resources" \
    --platform macosx \
    --minimum-deployment-target 14.0 \
    --app-icon AppIcon \
    --output-partial-info-plist /dev/null > /dev/null

# Codesign (stable identity so macOS permissions persist across rebuilds).
# Skipped for release builds (release.sh signs nested code inside-out with a
# Developer ID identity). Override the dev identity via CODESIGN_IDENTITY.
if [ "${SKIP_SIGN:-0}" != "1" ]; then
    SIGN_IDENTITY="${CODESIGN_IDENTITY:-Apple Development: dev@harke.me (L2GZ9Q95QW)}"
    codesign --force --sign "$SIGN_IDENTITY" --identifier me.harke.d-streamy.dstreamy "$APP/Contents/MacOS/dstreamy"
    codesign --force --sign "$SIGN_IDENTITY" --identifier me.harke.d-streamy "$APP"
fi

echo "==> Done: $APP"
