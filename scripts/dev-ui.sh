#!/bin/zsh
set -euo pipefail

SCRIPT_DIR=${0:A:h}
PROJECT_DIR=${SCRIPT_DIR:h}
DEV_HOME=$(mktemp -d "${TMPDIR:-/tmp}/provider-x-ui.XXXXXX")
trap 'rm -rf -- "$DEV_HOME"' EXIT INT TERM

cargo build --manifest-path "$PROJECT_DIR/Cargo.toml" -p provider-x-app --bin provider-x
print "ProviderX UI uses isolated temporary data. Edit crates/provider-x-app/ui to hot reload."
PROVIDER_X_TEST_HOME="$DEV_HOME" PROVIDER_X_UI_WATCH=1 \
  "$PROJECT_DIR/target/debug/provider-x" --show-settings
