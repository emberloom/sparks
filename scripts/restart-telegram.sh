#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="${ATHENA_ENV_FILE:-$ROOT_DIR/.env}"
ATHENA_BIN="${ATHENA_BIN:-$ROOT_DIR/target/debug/athena}"
LOG_FILE="${ATHENA_TELEGRAM_LOG:-$ROOT_DIR/athena_telegram.log}"

usage() {
  cat <<'EOF'
Usage: scripts/restart-telegram.sh

Restarts Athena Telegram bot.

Environment overrides:
  ATHENA_BIN           Path to Athena binary (default: ./target/debug/athena)
  ATHENA_TELEGRAM_LOG  Log file path (default: ./athena_telegram.log)
  ATHENA_ENV_FILE      Path to .env file (default: ./.env)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a
  . "$ENV_FILE"
  set +a
fi


if ! "$ATHENA_BIN" telegram --help >/dev/null 2>&1; then
  (cd "$ROOT_DIR" && cargo build --features telegram >/dev/null)
fi

existing_pids="$(pgrep -f "target/debug/athena telegram|cargo run --features telegram -- telegram" || true)"
if [ -n "$existing_pids" ]; then
  echo "Stopping existing telegram process(es): $existing_pids"
  kill $existing_pids || true
  sleep 1
fi

echo "Starting Telegram bot..."
nohup "$ATHENA_BIN" telegram >"$LOG_FILE" 2>&1 &
new_pid=$!

sleep 2
if ps -p "$new_pid" >/dev/null 2>&1; then
  echo "Telegram bot restarted successfully (pid=$new_pid)"
  echo "Log file: $LOG_FILE"
  exit 0
fi

echo "Telegram bot exited immediately. Recent logs:" >&2
tail -n 60 "$LOG_FILE" >&2 || true
exit 1
