#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

SUITE="${1:-$ROOT_DIR/eval/benchmark-real-gate.json}"
CONFIG="${2:-$ROOT_DIR/config.toml}"
ATHENA_BIN="${3:-$ROOT_DIR/target/debug/athena}"

if [[ ! -x "$ATHENA_BIN" ]]; then
  echo "Building Athena binary..."
  cargo build --bin athena
fi

python3 "$ROOT_DIR/scripts/eval_harness.py" \
  --suite "$SUITE" \
  --config "$CONFIG" \
  --athena-bin "$ATHENA_BIN"

python3 "$ROOT_DIR/scripts/eval_dashboard.py" \
  --config "$CONFIG" \
  --repo athena
