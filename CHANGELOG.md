# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- OpenAI-compatible API endpoints (`/v1/models`, `/v1/chat/completions`) with auth, rate limits, and docs.
- Ghost auto-specialization based on KPI outcomes with stability thresholds and rollback behavior.

### Changed

- Autonomous task routing now evaluates historical KPI outcomes when selecting a default ghost.

## [0.1.0] - 2026-02-26

### Added

- initial public repository release metadata and policy docs
- tag-based GitHub release workflow
- deterministic profile toggle via `ATHENA_DISABLE_HOME_PROFILES`

### Changed

- `athena ghosts` no longer requires LLM connectivity
- `doctor --ci` now treats optional self-improvement loops as warnings when not enabled
- maintainability baseline refreshed to current code layout

### Removed

- tracked runtime logs and local scratch artifact from repository history going forward
