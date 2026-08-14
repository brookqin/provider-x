#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
TARGET_DIR="$PROJECT_DIR/target/macos"
APP_DIR="$TARGET_DIR/ProviderX.app"
SIGN_IDENTITY=${PROVIDER_X_CODESIGN_IDENTITY:--}

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  print -u2 "provider-x v1 只构建 Apple Silicon macOS 应用"
  exit 1
fi

cargo build --manifest-path "$PROJECT_DIR/Cargo.toml" --release \
  --package provider-x-app --bin provider-x

mkdir -p "$TARGET_DIR"
if [[ -e "$APP_DIR" ]]; then
  case "$APP_DIR" in
    "$PROJECT_DIR"/target/macos/ProviderX.app) rm -rf -- "$APP_DIR" ;;
    *) print -u2 "拒绝清理意外路径: $APP_DIR"; exit 1 ;;
  esac
fi

mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$PROJECT_DIR/target/release/provider-x" "$APP_DIR/Contents/MacOS/provider-x"
cp "$PROJECT_DIR/crates/provider-x-app/resources/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "$PROJECT_DIR/LICENSE" "$APP_DIR/Contents/Resources/LICENSE"
cp "$PROJECT_DIR/crates/provider-x-app/resources/icons/LICENSE-LUCIDE" \
  "$APP_DIR/Contents/Resources/LICENSE-LUCIDE"
iconutil --convert icns \
  --output "$APP_DIR/Contents/Resources/AppIcon.icns" \
  "$PROJECT_DIR/crates/provider-x-app/resources/app-icon/AppIcon.iconset"
chmod 755 "$APP_DIR/Contents/MacOS/provider-x"
if [[ "$SIGN_IDENTITY" == "-" ]]; then
  codesign --force --sign - "$APP_DIR" >/dev/null
else
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP_DIR" >/dev/null
fi

"$SCRIPT_DIR/verify-macos-app.sh" "$APP_DIR" >&2

print "$APP_DIR"
