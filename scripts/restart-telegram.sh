#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${ATHENA_ENV_FILE:-$ROOT_DIR/.env}"
LOG_FILE="${ATHENA_TELEGRAM_LOG:-$ROOT_DIR/athena_telegram.log}"
ATHENA_BIN="${ATHENA_BIN:-$ROOT_DIR/target/debug/athena}"

if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a
  . "$ENV_FILE"
  set +a
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found on PATH" >&2
  exit 1
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

nohup "$ATHENA_BIN" telegram >"$LOG_FILE" 2>&1 &
echo "Telegram bot started. Logs: $LOG_FILE"
