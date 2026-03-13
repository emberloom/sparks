# Sparks Documentation

Documentation index for the Sparks multi-agent orchestration system.

---

## Architecture

| File | Description |
|---|---|
| [architecture.md](architecture.md) | Component overview, state machines, data-flow and pulse-delivery diagrams (Mermaid) |
| [self-improvement-architecture.md](self-improvement-architecture.md) | Self-improvement loop design: eval → optimizer → supervised patch cycle |
| [orchestration-layer-100-plan.md](orchestration-layer-100-plan.md) | Roadmap for the orchestration layer milestone |

---

## Contracts

| File | Description |
|---|---|
| [feature-contract-v1.md](feature-contract-v1.md) | Feature contract schema v1 — fields, validation rules, and lifecycle |
| [task-contract-v1.md](task-contract-v1.md) | Task contract schema v1 — atomic unit of ghost work |
| [execution-contract-v1.md](execution-contract-v1.md) | Execution contract v1 — how ghosts receive and report on tasks |
| [mission-contract.md](mission-contract.md) | Top-level mission contract describing system objectives |
| [feature-contract-workflow.md](feature-contract-workflow.md) | End-to-end workflow: create → dispatch → validate a feature contract |

---

## Evaluation

| File | Description |
|---|---|
| [eval-harness.md](eval-harness.md) | Eval harness design: scenario matrix, scoring, CI integration |
| [eval-smoke.md](eval-smoke.md) | Smoke eval scenarios for quick sanity checks |
| [optimizer-tournament.md](optimizer-tournament.md) | Optimizer tournament: how competing patches are ranked and selected |

---

## Operations & Observability

| File | Description |
|---|---|
| [maintainability-map.md](maintainability-map.md) | Maintainability scoring map — module-level metrics and targets |
| [local-only-deployment.md](local-only-deployment.md) | Fully local runtime profile (`local_only`) setup and red-team verification checklist |
| [observability-dashboard.md](observability-dashboard.md) | Static observability dashboard generation, lineage mapping, and CI/release artifacts |
| [hygiene-baseline.json](hygiene-baseline.json) | Hygiene check baseline (machine-generated, updated by `hygiene_check.py`) |
| [maintainability-baseline.json](maintainability-baseline.json) | Maintainability baseline (machine-generated) |
| [rust-audit-2026-03-02.md](rust-audit-2026-03-02.md) | Rust dependency security audit — 2026-03-02 |
| [security-attestation.md](security-attestation.md) | `sparks doctor --security` attestation schema, samples, and CI interpretation guide |
| [openai-compatible-api.md](openai-compatible-api.md) | OpenAI-compatible API setup, supported fields, and documented deviations |
| [mcp-integration.md](mcp-integration.md) | MCP server/tool configuration, namespacing, allowlists, and troubleshooting |
| [session-review-explainability.md](session-review-explainability.md) | Session activity log, explainability workflow, and Telegram activity commands |
| [ghost-specialization.md](ghost-specialization.md) | KPI-driven autonomous ghost selection policy and decision telemetry |
| [prompt-scanner.md](prompt-scanner.md) | Input-layer prompt scanner modes, thresholds, allowlists, and override behavior |
| [lane-test-note.md](lane-test-note.md) | Notes on KPI lane test coverage |
| [parsing-hardening-plan.md](parsing-hardening-plan.md) | Plan for hardening LLM-output parsing |

---

## Roadmap & Research

| File | Description |
|---|---|
| [self-improvement-roadmap.md](self-improvement-roadmap.md) | Phased self-improvement roadmap with acceptance criteria per phase |
| [self-build-supervised-batch.md](self-build-supervised-batch.md) | Supervised self-build batch execution design |
| [autonomous-agents-epic-comparison.md](autonomous-agents-epic-comparison.md) | Comparison of autonomous agent frameworks considered during design |
| [conductor-sparks-gap-analysis.md](conductor-sparks-gap-analysis.md) | Gap analysis: Conductor vs Sparks capability comparison |
| [telegram-planning-spec.md](telegram-planning-spec.md) | Telegram front-end feature specification |
