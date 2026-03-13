#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DURATION_SECS="${1:-28800}"  # default: 8h
INTERVAL_SECS="${2:-1800}"   # default: 30m
SOAK_NAME="${3:-overnight}"

START_TS="$(date -u +"%Y%m%dT%H%M%SZ")"
RUN_DIR="$ROOT_DIR/eval/results/soak-${SOAK_NAME}-${START_TS}"
mkdir -p "$RUN_DIR"
LAUNCH_LOG="$RUN_DIR/launch.log"
RUN_SCRIPT="$RUN_DIR/run.sh"

cat > "$RUN_SCRIPT" <<EOF
#!/usr/bin/env bash
set -euo pipefail
cd "$ROOT_DIR"
exec "$ROOT_DIR/scripts/soak-autonomy.sh" "$DURATION_SECS" "$INTERVAL_SECS" "$SOAK_NAME" "$RUN_DIR"
EOF
chmod +x "$RUN_SCRIPT"

if command -v screen >/dev/null 2>&1; then
  SESSION="sparks_soak_${SOAK_NAME}_${START_TS}"
  screen -dmS "$SESSION" "$RUN_SCRIPT"

  {
    echo "launcher=screen"
    echo "session=$SESSION"
    echo "submitted_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    echo "run_script=$RUN_SCRIPT"
  } > "$LAUNCH_LOG"

  echo "Soak started"
  echo "launcher=screen"
  echo "session=$SESSION"
  echo "run_dir=$RUN_DIR"
  echo "launch_log=$LAUNCH_LOG"
  exit 0
fi

# Fallback path
nohup "$ROOT_DIR/scripts/soak-autonomy.sh" \
  "$DURATION_SECS" \
  "$INTERVAL_SECS" \
  "$SOAK_NAME" \
  "$RUN_DIR" >"$LAUNCH_LOG" 2>&1 &

pid=$!
echo "$pid" > "$RUN_DIR/launcher.pid"
echo "Soak started"
echo "launcher=nohup"
echo "pid=$pid"
echo "run_dir=$RUN_DIR"
echo "launch_log=$LAUNCH_LOG"
