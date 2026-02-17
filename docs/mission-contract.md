# Athena Mission Contract

Date: 2026-02-17

## Mission Statement

Athena autonomously delivers backlog work across products and continuously improves her own capability, while maintaining quality and safety.

## Mission Contract (Measurable)

Athena can autonomously complete at least 70% of low-risk repository tasks with at least 95% verification pass rate, at most 5% rollback rate, and zero critical safety incidents.

This maps to:

- X = task success rate
- Y = verification pass rate
- Z = rollback rate
- Safety = zero critical incidents (secret leaks, destructive operations, policy bypass)

## Mission Lanes

- `delivery`: product and feature backlog execution across repos/products.
- `self_improvement`: reliability and capability improvements in Athena itself.

Both lanes are measured using the same KPI framework.

## KPI Contract

- `Task Success Rate (X)`:
  - Definition: `tasks_succeeded / tasks_started`
  - Goal: maximize.
- `Verification Pass Rate (Y)`:
  - Definition: `verifications_passed / verifications_total`
  - Goal: maximize.
- `Rollback Rate (Z)`:
  - Definition: `rollbacks / tasks_succeeded`
  - Goal: minimize.
- `Mean Time To Fix (Recovery)`:
  - Definition: average seconds from a failed code-change event to the next successful code-change event.
  - Goal: minimize.

## Autonomy Readiness Tiers

- `low risk` (safe refactors/docs/tests):
  - autonomy allowed when 14-day rolling metrics satisfy:
  - `task_success_rate >= 0.70`
  - `verification_pass_rate >= 0.95`
  - `rollback_rate <= 0.05`
  - `mttf <= 3600s`
  - `critical_safety_incidents = 0`
- `medium risk` (behavior changes with tests):
  - PR-only with human approval.
  - requires real-gate pass and task-level acceptance checks.
- `high risk` (security, data migration, production-critical):
  - human-led only.
  - Athena can draft/verify but cannot promote autonomously.

## Program Baseline (2026-02-16 to 2026-02-17)

Reality check against mission:

- strong agent/eval plumbing exists, and an initial optimizer tournament loop is implemented
- real quality gate now runs with strict per-task delivery minima
- recent real-gate baseline (2026-02-17) is passing (`overall_score=0.96`, 3/3 terminal successes)

Estimated maturity by layer (updated 2026-02-17):

- agent execution: ~85%
- evaluation: ~65%
- failure logging/telemetry: ~70%
- self-improvement optimizer loop: ~25%
- end-to-end closed loop: ~55-60%

See detailed roadmap: `docs/self-improvement-roadmap.md`.

Phase closeout evidence (2026-02-17):

- `eval/results/phase1-phase2-closeout-latest.md`
- `eval/results/phase3-closeout-latest.md`

## Operating Model (Contract Stack)

Athena should execute work using this explicit chain:

1. `Feature Contract` defines user outcome, architecture bounds, and acceptance criteria.
2. `Task Contracts` decompose the feature into a DAG with explicit dependencies and done criteria.
3. `Execution Contract` runs each task via normalized CLI wrappers and deterministic retry/fallback policy.
4. `Eval Gate` scores plan/execution/tests/diff and blocks promotions on failures.
5. `Promotion Policy` applies risk-tier rules (auto-merge only for low-risk high-confidence changes).

Contract templates:

- `docs/feature-contract-v1.md`
- `docs/task-contract-v1.md`
- `docs/execution-contract-v1.md`

## Phase Targets

Phase 1 (stabilize):

- task success rate `>= 40%`
- verification pass rate `>= 75%`
- rollback rate `<= 15%`
- mean time to fix `<= 48h`

Phase 2 (scale):

- task success rate `>= 60%`
- verification pass rate `>= 85%`
- rollback rate `<= 8%`
- mean time to fix `<= 24h`

Phase 3 (mission-ready):

- task success rate `>= 75%`
- verification pass rate `>= 92%`
- rollback rate `<= 3%`
- mean time to fix `<= 8h`

## Measurement Guardrails

- Track KPIs segmented by:
  - `lane` (`delivery` | `self_improvement`)
  - `repo` (product/repository label)
  - `risk_tier` (`low` | `medium` | `high`)
- Do not report only aggregate values; review by lane and risk tier.
- Keep explicit self-improvement capacity (recommended baseline: 20%) so self-improvement does not get starved by delivery.

## Tracking Current State vs Evolution

Use `athena kpi`:

- Current state:
  - `athena kpi status --lane self_improvement --repo athena --risk medium`
- Persist snapshot:
  - `athena kpi snapshot --lane self_improvement --repo athena --risk medium`
- Snapshot history (evolution):
  - `athena kpi history --lane self_improvement --repo athena --limit 30`

Recommended cadence:

- at least daily snapshots per active lane/repo
- always snapshot before and after major refactor/autonomy changes

Tagged attribution source:

- KPI values are derived from lane/risk-tagged autonomous task outcomes when available.
- Dispatch metadata can be set explicitly from CLI:
  - `athena dispatch --goal "<...>" --lane delivery --risk medium --repo athena`
- Background autonomous loops emit tagged outcomes under `self_improvement` by default.

## Langfuse Tracking

KPI evolution can be tracked in Langfuse.

- Command:
  - `athena kpi snapshot --lane self_improvement --repo athena --risk medium --langfuse`
- Export shape:
  - trace event `mission:kpi_snapshot`
  - tags include `mission`, `kpi`, lane, risk tier
  - output payload contains full KPI snapshot values

This gives a timeline in SQLite (`kpi_snapshots`) and in Langfuse for external observability dashboards.
