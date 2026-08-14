#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
APP_DIR=$($SCRIPT_DIR/build-macos-app.sh)
LOG_FILE=$(mktemp -t provider-x-shell-smoke.XXXXXX)
SECOND_LOG=$(mktemp -t provider-x-shell-second.XXXXXX)
SMOKE_HOME=$(mktemp -d "${TMPDIR%/}/provider-x-shell-home.XXXXXX")
APP_PID=""

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
  --show-settings --smoke-lifecycle --smoke-exit-after-ms=2500 >"$LOG_FILE" 2>&1 &
APP_PID=$!

for _ in {1..50}; do
  if grep -q "PROVIDER_X_SMOKE settings_window=open" "$LOG_FILE"; then
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

grep -q "PROVIDER_X_SMOKE tray=ready activation_policy=accessory" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE egress=ready address=127.0.0.1:" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE settings_window=open" "$LOG_FILE"

if HOME="$SMOKE_HOME" "$APP_DIR/Contents/MacOS/provider-x" \
  --smoke-lock-only >"$SECOND_LOG" 2>&1; then
  print -u2 "second provider-x instance unexpectedly started"
  exit 1
fi
grep -q "another provider-x instance already owns the application lock" "$SECOND_LOG"
[[ "$(stat -f %Lp "$SMOKE_HOME/Library/Application Support/dev.qiankun.provider-x")" == "700" ]]
[[ "$(stat -f %Lp "$SMOKE_HOME/Library/Application Support/dev.qiankun.provider-x/provider-x.lock")" == "600" ]]

RSS_KB=$(ps -o rss= -p "$APP_PID" | tr -d ' ')
wait "$APP_PID"
APP_PID=""
grep -q "PROVIDER_X_SMOKE lifecycle=quit" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_closed_process_alive" "$LOG_FILE"
grep -q "PROVIDER_X_SMOKE lifecycle=window_reopened" "$LOG_FILE"

EXECUTABLE_BYTES=$(stat -f %z "$APP_DIR/Contents/MacOS/provider-x")
print "shell smoke passed"
print "app=$APP_DIR"
print "idle_sample_rss_kb=$RSS_KB"
print "executable_bytes=$EXECUTABLE_BYTES"
