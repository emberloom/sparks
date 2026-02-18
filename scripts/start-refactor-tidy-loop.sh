#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DURATION_SECS="${1:-28800}"  # default: 8h
INTERVAL_SECS="${2:-900}"    # default: 15m
LOOP_NAME="${3:-refactor_tidy}"
TICKETS_FILE="${4:-$ROOT_DIR/eval/refactor-tidy-8h-tickets.txt}"

START_TS="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_DIR="$ROOT_DIR/eval/results/${LOOP_NAME}-${START_TS}"
mkdir -p "$RUN_DIR"

LAUNCH_LOG="$RUN_DIR/launch.log"
RUN_SCRIPT="$RUN_DIR/run.sh"

cat > "$RUN_SCRIPT" <<EOF
#!/usr/bin/env bash
set -euo pipefail
cd "$ROOT_DIR"
exec bash "$ROOT_DIR/scripts/refactor-tidy-loop.sh" "$DURATION_SECS" "$INTERVAL_SECS" "$LOOP_NAME" "$RUN_DIR" "$TICKETS_FILE"
EOF
chmod +x "$RUN_SCRIPT"

if command -v screen >/dev/null 2>&1; then
  SESSION="athena_refactor_${LOOP_NAME}_${START_TS}"
  screen -dmS "$SESSION" "$RUN_SCRIPT"
  {
    echo "launcher=screen"
    echo "session=$SESSION"
    echo "submitted_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    echo "run_dir=$RUN_DIR"
    echo "run_script=$RUN_SCRIPT"
  } > "$LAUNCH_LOG"
  echo "Refactor/tidy loop started"
  echo "launcher=screen"
  echo "session=$SESSION"
  echo "run_dir=$RUN_DIR"
  echo "launch_log=$LAUNCH_LOG"
  exit 0
fi

nohup "$ROOT_DIR/scripts/refactor-tidy-loop.sh" \
  "$DURATION_SECS" \
  "$INTERVAL_SECS" \
  "$LOOP_NAME" \
  "$RUN_DIR" \
  "$TICKETS_FILE" >"$LAUNCH_LOG" 2>&1 &

pid=$!
echo "$pid" > "$RUN_DIR/launcher.pid"
echo "Refactor/tidy loop started"
echo "launcher=nohup"
echo "pid=$pid"
echo "run_dir=$RUN_DIR"
echo "launch_log=$LAUNCH_LOG"
