# Contributing

## Development Setup

1. Install Rust toolchain from `rust-toolchain.toml`.
2. Install Python 3.11+.
3. Ensure Docker is running.
4. Copy config:
   - `cp config.example.toml config.toml`

## Before Opening a PR

Run the same checks used by CI:

- `cargo check -q`
- `cargo test -q`
- `cargo test -q --features telegram`
- `python3 scripts/test_eval_harness.py`
- `python3 scripts/test_optimizer_tournament.py`
- `python3 scripts/test_generate_improvement_backlog.py`
- `SPARKS_DISABLE_HOME_PROFILES=1 cargo run --quiet -- doctor --skip-llm --ci`
- `./scripts/maintainability_check.py`

## Deterministic Local Runs

If you use local profile ghosts under `~/.sparks/ghosts`, disable them for reproducible checks:

- `SPARKS_DISABLE_HOME_PROFILES=1 ...`

## Commit/PR Guidelines

- keep changes focused and reviewable
- include tests for behavior changes
- update docs when CLI flags or workflows change
- do not commit secrets, runtime DBs, or log artifacts
