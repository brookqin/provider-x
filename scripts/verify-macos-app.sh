#!/bin/zsh
set -euo pipefail

if [[ $# -ne 1 ]]; then
  print -u2 "usage: $0 /path/to/ProviderX.app"
  exit 2
fi

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
APP_DIR=${1:A}
EXECUTABLE="$APP_DIR/Contents/MacOS/provider-x"
PLIST="$APP_DIR/Contents/Info.plist"
LICENSE_FILE="$APP_DIR/Contents/Resources/LICENSE"
ICON="$APP_DIR/Contents/Resources/AppIcon.icns"
APP_VERSION=$(sed -nE 's/^version = "([^"]+)"/\1/p' "$PROJECT_DIR/Cargo.toml" | head -n 1)

[[ -d "$APP_DIR" ]] || { print -u2 "app bundle not found: $APP_DIR"; exit 1; }
[[ -x "$EXECUTABLE" ]] || { print -u2 "app executable missing: $EXECUTABLE"; exit 1; }
[[ -f "$PLIST" ]] || { print -u2 "Info.plist missing: $PLIST"; exit 1; }
[[ -f "$LICENSE_FILE" ]] || { print -u2 "GPL license missing: $LICENSE_FILE"; exit 1; }
[[ -f "$ICON" ]] || { print -u2 "app icon missing: $ICON"; exit 1; }
[[ -n "$APP_VERSION" ]] || { print -u2 "workspace version not found"; exit 1; }
grep -q "GNU GENERAL PUBLIC LICENSE" "$LICENSE_FILE"
grep -q "Version 3, 29 June 2007" "$LICENSE_FILE"

plutil -lint "$PLIST" >/dev/null
[[ "$(plutil -extract CFBundleExecutable raw "$PLIST")" == "provider-x" ]]
[[ "$(plutil -extract CFBundleIdentifier raw "$PLIST")" == "dev.qiankun.provider-x" ]]
[[ "$(plutil -extract CFBundleIconFile raw "$PLIST")" == "AppIcon.icns" ]]
[[ "$(plutil -extract LSUIElement raw "$PLIST")" == "true" ]]
[[ "$(plutil -extract CFBundleShortVersionString raw "$PLIST")" == "$APP_VERSION" ]]
[[ "$(plutil -extract CFBundleVersion raw "$PLIST")" == "$APP_VERSION" ]]

ICON_VERIFY_DIR=$(mktemp -d /tmp/provider-x-icon.XXXXXX)
cleanup() {
  case "$ICON_VERIFY_DIR" in
    /tmp/provider-x-icon.*) find "$ICON_VERIFY_DIR" -depth -delete ;;
    *) print -u2 "refusing to remove unexpected icon verification directory: $ICON_VERIFY_DIR" ;;
  esac
}
trap cleanup EXIT INT TERM
iconutil --convert iconset --output "$ICON_VERIFY_DIR/AppIcon.iconset" "$ICON"
[[ -f "$ICON_VERIFY_DIR/AppIcon.iconset/icon_512x512@2x.png" ]]

ARCHS=$(lipo -archs "$EXECUTABLE")
[[ "$ARCHS" == "arm64" ]] || {
  print -u2 "provider-x v1 must contain only arm64, found: $ARCHS"
  exit 1
}

codesign --verify --deep --strict --verbose=2 "$APP_DIR"

SIGNING_INFO=$(codesign --display --verbose=2 "$APP_DIR" 2>&1)
if [[ "$SIGNING_INFO" == *"Signature=adhoc"* ]]; then
  SIGNING_MODE=adhoc
else
  SIGNING_MODE=developer-id
fi

print "app verification passed"
print "app=$APP_DIR"
print "architectures=$ARCHS"
print "signing_mode=$SIGNING_MODE"
