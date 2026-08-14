#!/bin/zsh
set -euo pipefail
zmodload zsh/datetime

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
APP_DIR=$($SCRIPT_DIR/build-macos-app.sh)
NATIVE_BINARY="$PROJECT_DIR/target/release/examples/native-status-item"
APP_PID=""
NATIVE_PID=""
APP_LOG=$(mktemp -t provider-x-gpui-measure.XXXXXX)
NATIVE_LOG=$(mktemp -t provider-x-native-measure.XXXXXX)

cleanup() {
  for pid in "$APP_PID" "$NATIVE_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -f "$APP_LOG" "$NATIVE_LOG"
}
trap cleanup EXIT INT TERM

cargo build --manifest-path "$PROJECT_DIR/Cargo.toml" --release \
  --package provider-x-app --example native-status-item

APP_START=$EPOCHREALTIME
"$APP_DIR/Contents/MacOS/provider-x" >"$APP_LOG" 2>&1 &
APP_PID=$!
for _ in {1..50}; do
  grep -q "PROVIDER_X_SMOKE tray=ready" "$APP_LOG" && break
  sleep 0.1
done
grep -q "PROVIDER_X_SMOKE tray=ready" "$APP_LOG"
APP_READY=$EPOCHREALTIME
sleep 3
APP_RSS_KB=$(ps -o rss= -p "$APP_PID" | tr -d ' ')
APP_CPU_PERCENT=$(ps -o %cpu= -p "$APP_PID" | tr -d ' ')
APP_BYTES=$(stat -f %z "$APP_DIR/Contents/MacOS/provider-x")
APP_READY_MS=$(( (APP_READY - APP_START) * 1000 ))
kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
APP_PID=""

NATIVE_START=$EPOCHREALTIME
"$NATIVE_BINARY" >"$NATIVE_LOG" 2>&1 &
NATIVE_PID=$!
for _ in {1..50}; do
  grep -q "PROVIDER_X_NATIVE_SMOKE tray=ready" "$NATIVE_LOG" && break
  sleep 0.1
done
grep -q "PROVIDER_X_NATIVE_SMOKE tray=ready" "$NATIVE_LOG"
NATIVE_READY=$EPOCHREALTIME
sleep 3
NATIVE_RSS_KB=$(ps -o rss= -p "$NATIVE_PID" | tr -d ' ')
NATIVE_CPU_PERCENT=$(ps -o %cpu= -p "$NATIVE_PID" | tr -d ' ')
NATIVE_BYTES=$(stat -f %z "$NATIVE_BINARY")
NATIVE_READY_MS=$(( (NATIVE_READY - NATIVE_START) * 1000 ))
kill "$NATIVE_PID"
wait "$NATIVE_PID" 2>/dev/null || true
NATIVE_PID=""

print "gpui_tray_rss_kb=$APP_RSS_KB"
printf 'gpui_tray_ready_ms=%.0f\n' "$APP_READY_MS"
print "gpui_tray_cpu_percent=$APP_CPU_PERCENT"
print "gpui_tray_executable_bytes=$APP_BYTES"
print "native_status_item_rss_kb=$NATIVE_RSS_KB"
printf 'native_status_item_ready_ms=%.0f\n' "$NATIVE_READY_MS"
print "native_status_item_cpu_percent=$NATIVE_CPU_PERCENT"
print "native_status_item_executable_bytes=$NATIVE_BYTES"
