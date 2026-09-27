#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
APP_DIR="$PROJECT_DIR/target/macos/ProviderX.app"
OFFLINE_DIR=$(mktemp -d /tmp/provider-x-offline-smoke.XXXXXX)
OFFLINE_PID=""
cleanup() {
  local result=$?
  if [[ -n "$OFFLINE_PID" ]] && kill -0 "$OFFLINE_PID" 2>/dev/null; then
    kill "$OFFLINE_PID" 2>/dev/null || true
    wait "$OFFLINE_PID" 2>/dev/null || true
  fi
  if (( result != 0 )) && [[ -f "$OFFLINE_DIR/runtime.log" ]]; then
    cat "$OFFLINE_DIR/runtime.log" >&2
  fi
  case "$OFFLINE_DIR" in
    /tmp/provider-x-offline-smoke.*) rm -rf -- "$OFFLINE_DIR" ;;
  esac
}
trap cleanup EXIT INT TERM

"$SCRIPT_DIR/verify-macos-app.sh" "$APP_DIR" >&2
ditto "$APP_DIR" "$OFFLINE_DIR/ProviderX.app"
cd "$OFFLINE_DIR"
# GPUI needs local IPC; deny all non-loopback network access, source files and dependency cache.
PROVIDER_X_TEST_HOME="$OFFLINE_DIR/data" /usr/bin/sandbox-exec \
  -D SOURCE_ROOT="$PROJECT_DIR" -D DEPENDENCY_CACHE="$HOME/.gpui-shell/cache/dependencies" \
  -p '(version 1) (allow default)
      (deny network-outbound)
      (allow network-outbound (remote unix-socket))
      (allow network-outbound (remote ip "localhost:*"))
      (deny file-read* (subpath (param "SOURCE_ROOT")))
      (deny file-read* (subpath (param "DEPENDENCY_CACHE")))' \
  "$OFFLINE_DIR/ProviderX.app/Contents/MacOS/provider-x" \
  --smoke-lifecycle --smoke-exit-after-ms=7500 > "$OFFLINE_DIR/runtime.log" 2>&1 &
OFFLINE_PID=$!
wait "$OFFLINE_PID"
OFFLINE_PID=""
for EVENT in settings_script=loaded settings_window=open settings_window=released lifecycle=window_reopened lifecycle=quit; do
  grep -q "PROVIDER_X_SMOKE $EVENT" "$OFFLINE_DIR/runtime.log"
done
codesign --verify --deep --strict "$OFFLINE_DIR/ProviderX.app"
print "offline UI smoke passed: relocated bundle, external network/source/cache denied, signature unchanged"
