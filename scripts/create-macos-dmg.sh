#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
TARGET_DIR="$PROJECT_DIR/target/macos"
APP_DIR="$TARGET_DIR/ProviderX.app"
APP_VERSION=$(sed -nE 's/^version = "([^"]+)"/\1/p' "$PROJECT_DIR/Cargo.toml" | head -n 1)

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  print -u2 "provider-x v1 只打包 Apple Silicon macOS 应用"
  exit 1
fi

[[ -n "$APP_VERSION" ]] || {
  print -u2 "无法从 Cargo.toml 读取 workspace 版本"
  exit 1
}
[[ -d "$APP_DIR" ]] || {
  print -u2 "app bundle not found: $APP_DIR"
  print -u2 "run scripts/build-macos-app.sh first"
  exit 1
}

"$SCRIPT_DIR/verify-macos-app.sh" "$APP_DIR" >&2

DMG_PATH="$TARGET_DIR/ProviderX-$APP_VERSION-arm64.dmg"
STAGING_DIR=$(mktemp -d "$TARGET_DIR/dmg-staging.XXXXXX")
cleanup() {
  case "$STAGING_DIR" in
    "$TARGET_DIR"/dmg-staging.*) find "$STAGING_DIR" -depth -delete ;;
    *) print -u2 "refusing to remove unexpected DMG staging directory: $STAGING_DIR" ;;
  esac
}
trap cleanup EXIT INT TERM

ditto "$APP_DIR" "$STAGING_DIR/ProviderX.app"
ln -s /Applications "$STAGING_DIR/Applications"

case "$DMG_PATH" in
  "$TARGET_DIR"/ProviderX-*-arm64.dmg) rm -f -- "$DMG_PATH" ;;
  *) print -u2 "refusing to overwrite unexpected DMG path: $DMG_PATH"; exit 1 ;;
esac

hdiutil create \
  -volname "ProviderX $APP_VERSION" \
  -srcfolder "$STAGING_DIR" \
  -format UDZO \
  -ov \
  "$DMG_PATH" >&2

print "$DMG_PATH"
