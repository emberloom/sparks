#!/usr/bin/env bash
# basic-dispatch.sh — minimal Sparks workflow demo
#
# 1. Ensures config.toml exists
# 2. Runs doctor (no LLM required)
# 3. Lists ghost agents
# 4. Dispatches a simple task to the coder ghost
#
# Usage: ./examples/basic-dispatch.sh
# Requirements: Rust toolchain, Docker daemon running
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ── 1. Config ──────────────────────────────────────────────────────────────────
if [[ ! -f config.toml ]]; then
  echo "→ config.toml not found — copying from config.example.toml"
  cp config.example.toml config.toml
  echo "   Edit config.toml to set your LLM provider, then re-run."
  exit 1
fi

# ── 2. Doctor ─────────────────────────────────────────────────────────────────
echo ""
echo "── Doctor check (no LLM) ──────────────────────────────────────────────"
cargo run --quiet -- doctor --skip-llm

# ── 3. List ghosts ────────────────────────────────────────────────────────────
echo ""
echo "── Available ghost agents ──────────────────────────────────────────────"
SPARKS_DISABLE_HOME_PROFILES=1 cargo run --quiet -- ghosts

# ── 4. Dispatch task ──────────────────────────────────────────────────────────
echo ""
echo "── Dispatching sample task (wait up to 60s) ────────────────────────────"
cargo run --quiet -- dispatch \
  --goal "Print the string 'hello from Sparks' to stdout using a shell command." \
  --ghost coder \
  --wait-secs 60

echo ""
echo "Done. See docs/ for next steps."
