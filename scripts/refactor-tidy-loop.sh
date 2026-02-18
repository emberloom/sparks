#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

DURATION_SECS="${1:-28800}"     # default: 8h
INTERVAL_SECS="${2:-900}"       # default: 15m
LOOP_NAME="${3:-refactor_tidy}"
RUN_DIR_OVERRIDE="${4:-}"
TICKETS_FILE="${5:-$ROOT_DIR/eval/refactor-tidy-8h-tickets.txt}"
START_TS="$(date -u +"%Y%m%dT%H%M%SZ")"

if [[ -n "$RUN_DIR_OVERRIDE" ]]; then
  RUN_DIR="$RUN_DIR_OVERRIDE"
else
  RUN_DIR="$ROOT_DIR/eval/results/${LOOP_NAME}-${START_TS}"
fi
LOG_PATH="$RUN_DIR/loop.log"
STATE_FILE="$RUN_DIR/state.env"
PID_FILE="$RUN_DIR/loop.pid"
SUMMARY_MD="$RUN_DIR/summary.md"
MAINT_CURRENT_PATH="$ROOT_DIR/eval/results/maintainability-current.json"

ATHENA_BIN="${ATHENA_BIN:-$ROOT_DIR/target/debug/athena}"
CONFIG_PATH="${CONFIG_PATH:-$ROOT_DIR/config.toml}"
RISK="${RISK:-low}"
WAIT_SECS="${WAIT_SECS:-420}"
CLI_TOOL="${CLI_TOOL:-codex}"
CLI_MODEL="${CLI_MODEL:-}"
MAINTENANCE_PROFILE="${MAINTENANCE_PROFILE:-rust}"
PROMOTE_MODE="${PROMOTE_MODE:-}"
BASE_BRANCH="${BASE_BRANCH:-main}"

mkdir -p "$RUN_DIR"
echo "$$" > "$PID_FILE"

if [[ ! -f "$TICKETS_FILE" ]]; then
  echo "tickets file not found: $TICKETS_FILE" >&2
  exit 1
fi

if [[ ! -x "$ATHENA_BIN" ]]; then
  echo "Building athena binary..."
  cargo build -q --bin athena
fi

if [[ -z "$PROMOTE_MODE" ]]; then
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    PROMOTE_MODE="pr"
  else
    PROMOTE_MODE="none"
  fi
fi

TICKETS=()
while IFS= read -r line; do
  [[ "$line" =~ ^[[:space:]]*# ]] && continue
  [[ "$line" =~ ^[[:space:]]*$ ]] && continue
  TICKETS+=("$line")
done < "$TICKETS_FILE"
if (( ${#TICKETS[@]} == 0 )); then
  echo "no tickets found in: $TICKETS_FILE" >&2
  exit 1
fi

{
  echo "LOOP_NAME=$LOOP_NAME"
  echo "START_TS=$START_TS"
  echo "DURATION_SECS=$DURATION_SECS"
  echo "INTERVAL_SECS=$INTERVAL_SECS"
  echo "RUN_DIR=$RUN_DIR"
  echo "LOG_PATH=$LOG_PATH"
  echo "TICKETS_FILE=$TICKETS_FILE"
  echo "ATHENA_BIN=$ATHENA_BIN"
  echo "CONFIG_PATH=$CONFIG_PATH"
  echo "CLI_TOOL=$CLI_TOOL"
  echo "CLI_MODEL=$CLI_MODEL"
  echo "PROMOTE_MODE=$PROMOTE_MODE"
  echo "RISK=$RISK"
  echo "WAIT_SECS=$WAIT_SECS"
  echo "MAINTENANCE_PROFILE=$MAINTENANCE_PROFILE"
  echo "BASE_BRANCH=$BASE_BRANCH"
} > "$STATE_FILE"

pass_steps=0
fail_steps=0
iter=0
ticket_success=0
ticket_fail=0
end_epoch=$(( $(date +%s) + DURATION_SECS ))

run_step() {
  local label="$1"
  shift
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "[$ts] step=$label cmd=$*" | tee -a "$LOG_PATH"
  if "$@" >>"$LOG_PATH" 2>&1; then
    echo "[$ts] step=$label status=PASS" | tee -a "$LOG_PATH"
    pass_steps=$((pass_steps + 1))
    return 0
  fi
  echo "[$ts] step=$label status=FAIL" | tee -a "$LOG_PATH"
  fail_steps=$((fail_steps + 1))
  return 1
}

echo "Starting refactor/tidy loop: duration=${DURATION_SECS}s interval=${INTERVAL_SECS}s run_dir=$RUN_DIR" | tee -a "$LOG_PATH"
echo "ticket_count=${#TICKETS[@]} promote_mode=$PROMOTE_MODE cli_tool=$CLI_TOOL" | tee -a "$LOG_PATH"

while (( $(date +%s) < end_epoch )); do
  iter=$((iter + 1))
  iter_ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  ticket_idx=$(( (iter - 1) % ${#TICKETS[@]} ))
  ticket="${TICKETS[$ticket_idx]}"

  echo "" | tee -a "$LOG_PATH"
  echo "==== ITERATION $iter @ $iter_ts ====" | tee -a "$LOG_PATH"
  echo "ticket_index=$ticket_idx ticket=$ticket" | tee -a "$LOG_PATH"

  cmd=(
    "$ATHENA_BIN" --config "$CONFIG_PATH"
    self-build run
    --ticket "$ticket"
    --risk "$RISK"
    --wait-secs "$WAIT_SECS"
    --maintenance-profile "$MAINTENANCE_PROFILE"
    --promote-mode "$PROMOTE_MODE"
    --base-branch "$BASE_BRANCH"
    --cli-tool "$CLI_TOOL"
  )
  if [[ -n "$CLI_MODEL" ]]; then
    cmd+=(--cli-model "$CLI_MODEL")
  fi

  if run_step self_build "${cmd[@]}"; then
    ticket_success=$((ticket_success + 1))
  else
    ticket_fail=$((ticket_fail + 1))
  fi

  run_step maintainability_current \
    python3 "$ROOT_DIR/scripts/maintainability_check.py" \
      --root "$ROOT_DIR" \
      --json \
      --no-fail \
      --out "$MAINT_CURRENT_PATH" || true

  run_step improvement_backlog \
    python3 "$ROOT_DIR/scripts/generate_improvement_backlog.py" \
      --config "$CONFIG_PATH" \
      --maint-baseline "$MAINT_CURRENT_PATH" \
      --history-file "$ROOT_DIR/eval/results/history.jsonl" \
      --out-dir "$ROOT_DIR/eval/results" \
      --top 20 || true

  run_step doctor_ci \
    "$ATHENA_BIN" --config "$CONFIG_PATH" doctor --skip-llm --ci || true

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

{
  echo "# Refactor/Tidy Loop Summary"
  echo
  echo "- loop_name: \`$LOOP_NAME\`"
  echo "- started_utc: \`$START_TS\`"
  echo "- finished_utc: \`$done_ts\`"
  echo "- run_dir: \`$RUN_DIR\`"
  echo "- ticket_count: \`${#TICKETS[@]}\`"
  echo "- iterations: \`$iter\`"
  echo "- ticket_success: \`$ticket_success\`"
  echo "- ticket_fail: \`$ticket_fail\`"
  echo "- pass_steps: \`$pass_steps\`"
  echo "- fail_steps: \`$fail_steps\`"
  echo
  echo "## Inputs"
  echo
  echo "- tickets_file: \`$TICKETS_FILE\`"
  echo "- cli_tool: \`$CLI_TOOL\`"
  echo "- cli_model: \`$CLI_MODEL\`"
  echo "- promote_mode: \`$PROMOTE_MODE\`"
  echo "- risk: \`$RISK\`"
  echo "- wait_secs: \`$WAIT_SECS\`"
  echo "- maintenance_profile: \`$MAINTENANCE_PROFILE\`"
} > "$SUMMARY_MD"

echo "" | tee -a "$LOG_PATH"
echo "Refactor/tidy loop completed at $done_ts" | tee -a "$LOG_PATH"
echo "summary iterations=$iter ticket_success=$ticket_success ticket_fail=$ticket_fail pass_steps=$pass_steps fail_steps=$fail_steps" | tee -a "$LOG_PATH"
echo "run_dir=$RUN_DIR" | tee -a "$LOG_PATH"
echo "summary_md=$SUMMARY_MD" | tee -a "$LOG_PATH"
