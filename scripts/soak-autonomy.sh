#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DURATION_SECS="${1:-28800}"   # default: 8h
INTERVAL_SECS="${2:-1800}"    # default: 30m
SOAK_NAME="${3:-autonomy}"
RUN_DIR_OVERRIDE="${4:-}"
START_TS="$(date -u +"%Y%m%dT%H%M%SZ")"
if [[ -n "$RUN_DIR_OVERRIDE" ]]; then
  RUN_DIR="$RUN_DIR_OVERRIDE"
else
  RUN_DIR="$ROOT_DIR/eval/results/soak-${SOAK_NAME}-${START_TS}"
fi
LOG_PATH="$RUN_DIR/soak.log"
PID_FILE="$RUN_DIR/soak.pid"
STATE_FILE="$RUN_DIR/state.env"
MAINT_CURRENT_PATH="$ROOT_DIR/eval/results/maintainability-current.json"

mkdir -p "$RUN_DIR"

echo "$$" > "$PID_FILE"
{
  echo "SOAK_NAME=$SOAK_NAME"
  echo "START_TS=$START_TS"
  echo "DURATION_SECS=$DURATION_SECS"
  echo "INTERVAL_SECS=$INTERVAL_SECS"
  echo "RUN_DIR=$RUN_DIR"
  echo "LOG_PATH=$LOG_PATH"
} > "$STATE_FILE"

if [[ ! -x "$ROOT_DIR/target/debug/sparks" ]]; then
  echo "Building sparks binary..."
  cargo build -q --bin sparks
fi

pass_count=0
fail_count=0
iter=0
end_epoch=$(( $(date +%s) + DURATION_SECS ))

run_step() {
  local label="$1"
  shift
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "[$ts] step=$label cmd=$*" | tee -a "$LOG_PATH"
  if "$@" >> "$LOG_PATH" 2>&1; then
    echo "[$ts] step=$label status=PASS" | tee -a "$LOG_PATH"
    pass_count=$((pass_count + 1))
    return 0
  fi
  echo "[$ts] step=$label status=FAIL" | tee -a "$LOG_PATH"
  fail_count=$((fail_count + 1))
  return 1
}

echo "Starting soak run: duration=${DURATION_SECS}s interval=${INTERVAL_SECS}s run_dir=$RUN_DIR" | tee -a "$LOG_PATH"

while (( $(date +%s) < end_epoch )); do
  iter=$((iter + 1))
  iter_ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "" | tee -a "$LOG_PATH"
  echo "==== ITERATION $iter @ $iter_ts ====" | tee -a "$LOG_PATH"

  run_step doctor "$ROOT_DIR/target/debug/sparks" --config "$ROOT_DIR/config.toml" doctor --skip-llm --ci || true

  run_step eval_cli_matrix \
    python3 "$ROOT_DIR/scripts/eval_cli_matrix.py" \
      --suite "$ROOT_DIR/eval/benchmark-cli-smoke.json" \
      --config "$ROOT_DIR/config.toml" \
      --sparks-bin "$ROOT_DIR/target/debug/sparks" \
      --dispatch-context "[benchmark_fast_cli]" \
      --cli-timeout-secs 120 || true

  run_step kpi_self_improvement_low \
    "$ROOT_DIR/target/debug/sparks" --config "$ROOT_DIR/config.toml" \
      kpi snapshot --lane self_improvement --repo sparks --risk low || true

  run_step kpi_delivery_low \
    "$ROOT_DIR/target/debug/sparks" --config "$ROOT_DIR/config.toml" \
      kpi snapshot --lane delivery --repo sparks --risk low || true

  run_step eval_dashboard \
    python3 "$ROOT_DIR/scripts/eval_dashboard.py" \
      --config "$ROOT_DIR/config.toml" \
      --repo sparks || true

  run_step maintainability_current \
    python3 "$ROOT_DIR/scripts/maintainability_check.py" \
      --root "$ROOT_DIR" \
      --json \
      --no-fail \
      --out "$MAINT_CURRENT_PATH" || true

  run_step improvement_backlog \
    python3 "$ROOT_DIR/scripts/generate_improvement_backlog.py" \
      --config "$ROOT_DIR/config.toml" \
      --maint-baseline "$MAINT_CURRENT_PATH" \
      --history-file "$ROOT_DIR/eval/results/history.jsonl" \
      --out-dir "$ROOT_DIR/eval/results" \
      --top 20 || true

  now_epoch="$(date +%s)"
  if (( now_epoch >= end_epoch )); then
    break
  fi
  sleep_for=$INTERVAL_SECS
  remaining=$(( end_epoch - now_epoch ))
  if (( sleep_for > remaining )); then
    sleep_for=$remaining
  fi
  echo "Sleeping ${sleep_for}s before next iteration..." | tee -a "$LOG_PATH"
  sleep "$sleep_for"
done

done_ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
run_step soak_summary \
  python3 "$ROOT_DIR/scripts/generate_soak_summary.py" \
    --run-dir "$RUN_DIR" \
    --results-dir "$ROOT_DIR/eval/results" \
    --out "$RUN_DIR/summary.md" || true

echo "" | tee -a "$LOG_PATH"
echo "Soak completed at $done_ts" | tee -a "$LOG_PATH"
echo "summary pass_steps=$pass_count fail_steps=$fail_count iterations=$iter" | tee -a "$LOG_PATH"
echo "run_dir=$RUN_DIR" | tee -a "$LOG_PATH"
