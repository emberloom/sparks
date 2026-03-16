#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

SUITE="${1:-$ROOT_DIR/eval/benchmark-real-gate.json}"
CONFIG="${2:-$ROOT_DIR/config.toml}"
SPARKS_BIN="${3:-$ROOT_DIR/target/debug/sparks}"

if [[ ! -x "$SPARKS_BIN" ]]; then
  echo "Building Sparks binary..."
  cargo build --bin sparks
fi

python3 "$ROOT_DIR/scripts/eval_harness.py" \
  --suite "$SUITE" \
  --config "$CONFIG" \
  --sparks-bin "$SPARKS_BIN"

python3 "$ROOT_DIR/scripts/eval_dashboard.py" \
  --config "$CONFIG" \
  --repo sparks
