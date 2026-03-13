#!/usr/bin/env bash
set -euo pipefail

DURATION_HOURS="${1:-24}"
INTERVAL_SECS="${2:-3600}"
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN_PATH="$ROOT_DIR/target/debug/sparks"
LOG_PATH="$ROOT_DIR/sparks_doctor_soak.log"

if [[ ! -x "$BIN_PATH" ]]; then
  echo "Building sparks binary..."
  (cd "$ROOT_DIR" && cargo build --quiet --bin sparks)
fi

runs=$(( (DURATION_HOURS * 3600 + INTERVAL_SECS - 1) / INTERVAL_SECS ))

echo "Starting soak: hours=$DURATION_HOURS interval=${INTERVAL_SECS}s runs=$runs" | tee -a "$LOG_PATH"

for ((i=1; i<=runs; i++)); do
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  {
    echo "==== SOAK RUN $i/$runs @ $ts ===="
    "$BIN_PATH" doctor --skip-llm --ci
    echo "status=PASS"
    echo
  } >> "$LOG_PATH" 2>&1 || {
    {
      echo "status=FAIL"
      echo
    } >> "$LOG_PATH"
  }

  if (( i < runs )); then
    sleep "$INTERVAL_SECS"
  fi
done

echo "Soak completed at $(date -u +"%Y-%m-%dT%H:%M:%SZ")" | tee -a "$LOG_PATH"
