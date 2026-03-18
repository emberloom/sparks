# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

## [0.3.0] - 2026-03-18

### Added

- **Slack integration** (`--features slack`) — Socket Mode bot with slash commands (`/sparks help|status|plan|implement|review|explain|search|alerts|ghosts|memories|dispatch|model|jobs|mood|knobs|session|cli|set|watch`), Block Kit planning interview (5-step), streaming responses, per-channel auth and rate limiting, confirmations with approve/deny buttons, and pulse delivery.
- **Microsoft Teams integration** (`--features teams`) — Bot Framework REST API with JWT RS256 signature verification, `serviceUrl` validation, tenant authorization, Adaptive Cards for confirmations and planning interview, bearer token cache, and all equivalent commands.
- **Proactive alerting engine** — Rule-based alert evaluation against the activity log with multi-channel delivery (`log`, `slack`, `teams`, `webhook`), configurable severity thresholds, and silence windows (`[alerts]` config section).
- **Semantic memory deduplication and decay scoring** — Exponential decay scoring with configurable half-life, soft entry cap, and deduplication pass to prune near-duplicate memories.
- **SonarQube MCP quality gate integration** — `sonarqube_gate.py` script and `SonarqubeConfig` for polling SonarCloud/self-hosted quality gates; optionally blocks on failure (`[sonarqube]` config section).
- **Workspace snapshot and time-travel debugging** — `sparks snapshot create|list|diff|restore` with configurable retention, size guard, include/exclude globs, and atomic restore (`[snapshot]` config section).
- **Ghost performance leaderboard and A/B testing** — `sparks leaderboard show|compare` with success-rate ranking, configurable A/B routing fraction, and promotion recommendations (`[leaderboard]` config section).

### Changed

- `SPARKS_SLACK_BOT_TOKEN`, `SPARKS_SLACK_APP_TOKEN`, `SPARKS_SLACK_SIGNING_SECRET` added to the secrets registry (keyring + env var support via `sparks secrets set slack.*`).
- Complete Athena→Sparks rebrand pass: env vars, binary references, config paths (`~/.sparks/`), CLI contract tag (`[sparks_cli_contract]`), and model names across all docs and configs.

## [0.2.0] - 2026-03-13

### Changed

- **Rebrand: Athena → Emberloom Sparks.** Full codebase rebrand across 132 files — crate name, error types, env vars (`ATHENA_*` → `SPARKS_*`), config paths (`~/.athena/` → `~/.sparks/`), CLI contract markers, docs, CI workflows, scripts, and eval configs.
- GitHub repository moved from `Enreign/athena` to `emberloom/sparks`.
- Removed old Athena banner image from README.

## [0.1.2] - 2026-03-09

### Fixed

- Release workflow: replace `softprops/action-gh-release` with `gh` CLI to prevent `target_commitish` validation errors on re-runs against an already-published release.
- Release workflow: add `tag` input to `workflow_dispatch` so releases can be re-triggered from `main` for any tag.

## [0.1.1] - 2026-03-08

### Added

- GitHub wiki: 16 pages covering architecture, sparks, memory, MCP, observability, CLI reference, troubleshooting, and contributing guidelines.
- CodeQL security scanning workflow (weekly + on every PR) with Rust taint analysis.
- `cargo audit` step in CI to catch dependency CVEs on every push.
- `agents.md`: wiki update guideline (step 10) and CodeQL false-positive dismissal policy.

### Security

- Dismissed 3 CodeQL false-positive alerts with documented rationale: HTTPS guard in `LangfuseClient::new()` already validates base URL; `constant_time_eq` test inputs misidentified as cryptographic keys.

- OpenAI-compatible API endpoints (`/v1/models`, `/v1/chat/completions`) with auth, rate limits, and docs.
- Spark auto-specialization based on KPI outcomes with stability thresholds and rollback behavior.
- Session review and explainability system with activity-log persistence.
- Telegram activity commands: `/review`, `/explain`, `/watch`, `/search`, `/alerts`.
- MCP ToolRegistry wiring with namespaced tools (`mcp:<server>:<tool>`) and allowlist controls.
- Prompt scanner at chat/autonomous intake with `flag_only`/`block` modes and allowlist overrides.
- Tool-call loop guard circuit breaker (`manager.loop_guard`) to stop repeated call loops.
- Adaptive pre-dispatch token/context budgeting for oversized task contracts.
- HNSW semantic memory index with exact-cosine fallback for small/early datasets.
- Eval CLI wiring and scenario library manifest support.

### Changed

- Autonomous task routing now evaluates historical KPI outcomes when selecting a default spark.
- Code-index/scout overlap cleanup: indexing remains proactive, with overlapping paths removed.

## [0.1.0] - 2026-02-26

### Added

- initial public repository release metadata and policy docs
- tag-based GitHub release workflow
- deterministic profile toggle via `SPARKS_DISABLE_HOME_PROFILES`

### Changed

- `sparks ghosts` no longer requires LLM connectivity
- `doctor --ci` now treats optional self-improvement loops as warnings when not enabled
- maintainability baseline refreshed to current code layout

### Removed

- tracked runtime logs and local scratch artifact from repository history going forward
