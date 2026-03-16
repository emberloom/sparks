# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

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
