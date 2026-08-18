#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
APP_DIR=$($SCRIPT_DIR/build-macos-app.sh)
LOG_FILE=$(mktemp -t provider-x-shell-smoke.XXXXXX)
SECOND_LOG=$(mktemp -t provider-x-shell-second.XXXXXX)
SMOKE_HOME=$(mktemp -d "${TMPDIR%/}/provider-x-shell-home.XXXXXX")
APP_PID=""

physical_footprint_mb() {
  /usr/bin/footprint "$1" 2>/dev/null \
    | sed -n 's/^[[:space:]]*phys_footprint:[[:space:]]*\([0-9][0-9]*\) MB.*$/\1/p' \
    | head -n 1
}

cleanup() {
  if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
  fi
  rm -f "$LOG_FILE" "$SECOND_LOG"
  case "$SMOKE_HOME" in
    "${TMPDIR%/}"/provider-x-shell-home.*) find "$SMOKE_HOME" -depth -delete ;;
    *) print -u2 "refusing to remove unexpected smoke home: $SMOKE_HOME" ;;
  esac
}
trap cleanup EXIT INT TERM

HOME="$SMOKE_HOME" "$APP_DIR/Contents/MacOS/provider-x" \
  --smoke-lifecycle --smoke-exit-after-ms=7500 >"$LOG_FILE" 2>&1 &
APP_PID=$!

for _ in {1..50}; do
  if grep -q "PROVIDER_X_SMOKE tray=ready" "$LOG_FILE"; then
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

grep -q "PROVIDER_X_SMOKE tray=ready activation_policy=accessory" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE egress=ready address=127.0.0.1:" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE settings_ui=deferred" "$LOG_FILE"
DEFERRED_RSS_KB=$(ps -o rss= -p "$APP_PID" | tr -d ' ')
DEFERRED_FOOTPRINT_MB=$(physical_footprint_mb "$APP_PID")
for _ in {1..50}; do
  if grep -q "PROVIDER_X_SMOKE settings_window=open" "$LOG_FILE"; then
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
grep -q "PROVIDER_X_SMOKE settings_ui=initialized" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE settings_window=open" "$LOG_FILE"
OPEN_UI_RSS_KB=$(ps -o rss= -p "$APP_PID" | tr -d ' ')
OPEN_UI_FOOTPRINT_MB=$(physical_footprint_mb "$APP_PID")
EGRESS_ADDRESS=$(sed -n 's/^PROVIDER_X_SMOKE egress=ready address=\(127\.0\.0\.1:[0-9][0-9]*\)$/\1/p' "$LOG_FILE" | tail -n 1)
[[ -n "$EGRESS_ADDRESS" ]]
ERROR_STATUS=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  "http://$EGRESS_ADDRESS/v1/responses")
[[ "$ERROR_STATUS" == "404" ]]
ERROR_STATUS=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  "http://$EGRESS_ADDRESS/not-an-api-path")
[[ "$ERROR_STATUS" == "404" ]]

if HOME="$SMOKE_HOME" "$APP_DIR/Contents/MacOS/provider-x" \
  --smoke-lock-only >"$SECOND_LOG" 2>&1; then
  print -u2 "second provider-x instance unexpectedly started"
  exit 1
fi
grep -q "another provider-x instance already owns the application lock" "$SECOND_LOG"
[[ "$(stat -f %Lp "$SMOKE_HOME/Library/Application Support/dev.qiankun.provider-x")" == "700" ]]
[[ "$(stat -f %Lp "$SMOKE_HOME/Library/Application Support/dev.qiankun.provider-x/provider-x.lock")" == "600" ]]
SMOKE_LOG_DIR="$SMOKE_HOME/Library/Application Support/dev.qiankun.provider-x/logs"
[[ "$(stat -f %Lp "$SMOKE_LOG_DIR")" == "700" ]]
SMOKE_LOG_FILES=("$SMOKE_LOG_DIR"/provider-x-*.log(N))
[[ ${#SMOKE_LOG_FILES[@]} -eq 1 ]]
[[ "$(stat -f %Lp "$SMOKE_LOG_FILES[1]")" == "600" ]]
for _ in {1..20}; do
  if grep -q '"code":"ingress_not_found"' "$SMOKE_LOG_FILES[1]"; then
    break
  fi
  sleep 0.05
done
grep -q '"code":"ingress_not_found"' "$SMOKE_LOG_FILES[1]"
grep -q '"path":"/v1/responses"' "$SMOKE_LOG_FILES[1]"
grep -q '"path":"/not-an-api-path"' "$SMOKE_LOG_FILES[1]"
grep -q '"ingress_authorized":false' "$SMOKE_LOG_FILES[1]"

for _ in {1..50}; do
  if grep -q "PROVIDER_X_SMOKE lifecycle=window_closed_process_alive" "$LOG_FILE"; then
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
grep -q "PROVIDER_X_SMOKE settings_window=released" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_closed_process_alive" "$LOG_FILE"
sleep 4
RELEASED_UI_RSS_KB=$(ps -o rss= -p "$APP_PID" | tr -d ' ')
RELEASED_UI_FOOTPRINT_MB=$(physical_footprint_mb "$APP_PID")
(( OPEN_UI_FOOTPRINT_MB - DEFERRED_FOOTPRINT_MB >= 20 ))
(( OPEN_UI_FOOTPRINT_MB - RELEASED_UI_FOOTPRINT_MB >= 20 ))
wait "$APP_PID"
APP_PID=""
grep -q "PROVIDER_X_SMOKE lifecycle=quit" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_hidden_pending_release" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_reopened_before_release" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE settings_window=released" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_closed_process_alive" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_reopened" "$LOG_FILE"

EXECUTABLE_BYTES=$(stat -f %z "$APP_DIR/Contents/MacOS/provider-x")
print "shell smoke passed"
print "app=$APP_DIR"
print "deferred_ui_rss_kb=$DEFERRED_RSS_KB"
print "deferred_ui_footprint_mb=$DEFERRED_FOOTPRINT_MB"
print "open_ui_rss_kb=$OPEN_UI_RSS_KB"
print "open_ui_footprint_mb=$OPEN_UI_FOOTPRINT_MB"
print "released_ui_rss_kb=$RELEASED_UI_RSS_KB"
print "released_ui_footprint_mb=$RELEASED_UI_FOOTPRINT_MB"
print "executable_bytes=$EXECUTABLE_BYTES"
