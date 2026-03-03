# Athena

Secure autonomous multi-agent system for code execution, evaluation, and policy-bounded self-improvement.

## What Athena Includes

- multi-ghost task execution (`coder`, `scout`, feature/task contracts)
- guarded autonomous dispatch with outcome tracking
- memory and embedding-backed context
- doctor + KPI + eval harness pipelines
- supervised self-build and optimizer tournament tooling

## Requirements

- Rust toolchain (pinned via `rust-toolchain.toml`)
- Python 3.11+ for `scripts/*.py`
- Docker daemon (for containerized ghost execution)
- one configured LLM provider:
  - OpenAI (default), or
  - local Ollama / OpenRouter / Zen compatible endpoint

## Quickstart

1. Clone and enter repository.
2. Copy config template:
   - `cp config.example.toml config.toml`
3. Run baseline checks:
   - `cargo check -q`
   - `cargo test -q`
   - `cargo run --quiet -- doctor --skip-llm`
4. Start CLI:
   - `cargo run -- chat`

Useful no-network / deterministic local mode:

- `ATHENA_DISABLE_HOME_PROFILES=1 cargo run -- ghosts`

This disables `~/.athena/ghosts/*.toml` overrides so behavior only depends on repository config.

Fully local deployment profile + verification:

- [docs/local-only-deployment.md](docs/local-only-deployment.md)
- local-only doctor gate:
  - `ATHENA_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --skip-llm --ci --fail-on-warn`

## Common Commands

- list ghosts: `cargo run -- ghosts`
- dispatch one task: `cargo run -- dispatch --goal "..." --wait-secs 120`
- doctor report: `cargo run -- doctor --skip-llm`
- security attestation (text): `cargo run -- doctor --security`
- security attestation (JSON): `cargo run -- doctor --security --json`
- KPI snapshot: `cargo run -- kpi snapshot --lane delivery`
- dashboard (markdown/html): `cargo run -- dashboard --output-format html`
- user-flow harness (Linear intake + writeback): `make user-flow`
- feature contract flow: `cargo run -- feature --help`
- self-build flow: `cargo run -- self-build --help`

## CI

Main CI checks are in:

- `.github/workflows/maintainability.yml`
- `.github/workflows/eval-harness.yml`
- `.github/workflows/doctor.yml`

Real-gate and nightly optimizer workflows are intentionally self-hosted.

## Documentation

- `docs/self-improvement-roadmap.md`
- `docs/self-improvement-architecture.md`
- `docs/eval-harness.md`
- `docs/feature-contract-workflow.md`
- `docs/local-only-deployment.md`
- `docs/security-attestation.md`

## Local Secrets

Use a gitignored `.env` file (loaded automatically) for secrets like `ATHENA_TELEGRAM_TOKEN` and `GH_TOKEN`.
You can start the Telegram bot via `scripts/restart-telegram.sh`, which sources `.env` by default.

## Release

Tag-based GitHub release workflow:

- `.github/workflows/release.yml`

Create a tag like `v0.1.0` and push it to publish release artifacts.

## Project Policies

- Security: `SECURITY.md`
- Contributing: `CONTRIBUTING.md`
- Changelog: `CHANGELOG.md`
- License: `LICENSE`
