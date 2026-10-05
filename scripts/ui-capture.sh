#!/usr/bin/env bash
# Render Wire's UI to PNGs without screen-recording permission.
#
# usage: scripts/ui-capture.sh <out-dir> [scene-filter]
#
# Builds the release app, then runs it with WIRE_UI_CAPTURE set: the app loads
# in-memory fixture contacts/messages, walks every scene in
# wire-app/src/app/ui_capture.rs (window sizes, themes, modes, dialogs), saves
# one PNG per scene, and quits. A throwaway WIRE_CONFIG_DIR keeps your real
# profile, chats, and settings untouched.
#
# Examples:
#   scripts/ui-capture.sh .amp/in/artifacts/before
#   scripts/ui-capture.sh /tmp/shots calls-      # only scenes containing "calls-"
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <out-dir> [scene-filter]" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$1"
filter="${2:-}"
mkdir -p "$out"

if [[ ! -d "$repo_root/wire-app/fonts" ]]; then
  echo "note: wire-app/fonts/ is missing; captures will use egui's fallback font" >&2
fi

cargo build --release -p wire-app --manifest-path "$repo_root/Cargo.toml"

config_dir="$(mktemp -d "${TMPDIR:-/tmp}/wire-ui-capture.XXXXXX")"
trap 'rm -rf "$config_dir"' EXIT

status=0
WIRE_CONFIG_DIR="$config_dir" \
WIRE_UI_CAPTURE="$out" \
WIRE_UI_CAPTURE_FILTER="$filter" \
  timeout 300 "$repo_root/target/release/wire-app" >"$out/run.log" 2>&1 || status=$?

count=$(find "$out" -maxdepth 1 -name '*.png' | wc -l | tr -d ' ')
echo "captured $count scene(s) into $out (exit $status, log: $out/run.log)"
exit "$status"
