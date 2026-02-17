# Athena Self-Improvement Roadmap

Date: 2026-02-17

## North Star

Build Athena into a spec-driven engineering agent that can reliably deliver backlog work across products and improve itself through measured, policy-bounded iteration.

## Current State (Baseline)

As of 2026-02-16 to 2026-02-17:

- strong coding-agent backbone exists across `claude_code`, `codex`, and `opencode`
- autonomous task loop, outcomes, and memory logging exist
- eval harness, matrix runs, history, and dashboard exist
- strict real-gate scoring now exists, including task-level delivery minima

Key gaps versus target:

- no OpenEvolve-style prompt and skill mutation/selection loop
- no candidate tournament that auto-promotes best policy/prompt variant
- limited feature-level orchestration from one feature spec into multiple dependent tasks
- CI real gate remains manual/self-hosted, not always-on for every change

Maturity estimate:

- agent execution layer: ~85%
- evaluation layer: ~65%
- failure logging/telemetry layer: ~70%
- self-improvement optimizer: ~10%
- end-to-end closed loop (execute -> evaluate -> evolve -> promote): ~45-55%

## Operating Model (Spec-Driven)

Execution must follow one contract chain:

1. `Feature Contract`: user outcome, architecture constraints, acceptance criteria.
2. `Task Contracts`: DAG decomposition with dependencies and atomic done criteria.
3. `Execution Contract`: normalized CLI interface, deterministic error taxonomy, retry/fallback policy.
4. `Eval Gate`: benchmark and task-level scoring, strict promotion blocker.
5. `Promotion Policy`: risk-tiered auto-merge vs PR-only.

References:

- `docs/feature-contract-v1.md`
- `docs/task-contract-v1.md`
- `docs/execution-contract-v1.md`

## Roadmap Phases

### Phase 1: Reliability Baseline (Closed 2026-02-17)

Goal: make autonomous execution terminal, deterministic, and measurable.

Deliverables:

- strict real-gate baseline for quality decisions
- deterministic terminal outcome taxonomy (`dispatch_timeout`, `outcome_wait_timeout`, `stale_started`)
- normalized CLI contract tags and deterministic retry/fallback logic
- complete artifact and memory logging per terminal run

Exit criteria:

- 14 consecutive scheduled runs without unresolved `started` outcomes
- real gate stable and used for promotion decisions
- CLI contract replay determinism for known error fixtures

Closure evidence:

- closeout report: `eval/results/phase1-phase2-closeout-latest.md`
- latest real gate: `eval/results/eval-20260217T141900Z.json` (`gate_ok=true`)
- promotion policy now consumes latest real-gate status from `eval/results/history.jsonl`

### Phase 2: Feature-Level Orchestration (Closed 2026-02-17)

Goal: scale from single tasks to coherent multi-task feature delivery.

Deliverables:

- feature spec to task DAG planner
- dependency-aware scheduler (parallel where possible, serialized on dependencies)
- acceptance traceability map (`feature criterion -> task -> evidence`)
- integration gate after each merged task and at feature completion

Current implementation status:

- initial contract ingestion and DAG-ordered dispatch scaffolding is available via:
  - `athena feature validate --file <contract>`
  - `athena feature plan --file <contract>`
  - `athena feature dispatch --file <contract>`
  - `athena feature verify --file <contract>`
  - `athena feature promote --file <contract>`
  - `athena feature gate --file <contract> --verify-profile <fast|strict>`
- acceptance traceability floor is implemented:
  - required `acceptance_criteria[]` in feature contracts
  - required per-task `mapped_acceptance[]`
  - validation enforces full acceptance coverage by enabled tasks
- feature dispatch and verify emit evidence ledgers under:
  - `eval/results/feature-*.{json,md}`
  - `eval/results/feature-verify-*.{json,md}`
- feature promotion decisions emit supervised policy artifacts:
  - `eval/results/feature-promote-*.{json,md}`
- one-shot gate emits consolidated artifacts:
  - `eval/results/feature-gate-*.{json,md}`
- feature dispatch timeout handling now uses adaptive DB terminal-outcome grace reconciliation (risk/task-profile aware, override with `--outcome-grace-secs`) to reduce false timeout failures
- reference workflow: `docs/feature-contract-workflow.md`

Exit criteria:

- at least 5 features delivered through contract-driven DAG flow
- 100% feature acceptance criteria linked to objective evidence artifacts

Closure evidence:

- closeout run batch: `eval/results/phase2-closeout-runs-20260217T141929Z.json`
- gate artifacts: `eval/results/feature-gate-athena-phase2-closeout-*.json` (`5/5 gate_ok=true`)
- summary report: `eval/results/phase1-phase2-closeout-latest.md`

### Phase 3: Supervised Self-Build Pipeline

Goal: Athena can improve Athena in a bounded loop.

Deliverables:

- loop: detect issue -> propose patch -> isolated worktree implementation -> maintenance pack -> critic review -> promote decision
- `athena self-build run` now emits supervised promotion execution artifacts with explicit mode:
  - `--promote-mode none|pr|auto`
  - `--base-branch <branch>` for PR target
  - low-risk + high-confidence may auto-merge only in `auto` mode
  - medium/high-risk always stays PR-only
- guardrail policy now emits explicit violation classes (`policy.guardrail.<code>`) and fail-fast promotion policy output:
  - `self_build_policy promotion_allowed=<bool> reason_codes=<csv>`
- supervised PR critic checklist is emitted before merge attempts:
  - `eval/results/self-build-review-*.{json,md}` (risk, blast radius, rollback plan, blockers)
- supervised batch runner is prepared:
  - `scripts/supervised_self_build_batch.py`
  - operator guide: `docs/self-build-supervised-batch.md`
- promotion matrix:
  - low-risk high-confidence: auto-merge allowed
  - medium/high risk: PR-only human approval
- hard guardrails enforced by policy:
  - no secret reads in autonomous patch loop
  - no destructive git operations
  - no direct edits to protected branches

Exit criteria:

- 20 supervised self-build runs with zero guardrail violations
- measurable KPI lift from self-improvement lane without delivery regression

### Phase 4: Optimizer Loop (OpenEvolve-Style)

Goal: continuous prompt, policy, and skill evolution from benchmark feedback.

Deliverables:

- candidate generation from failures and maintainability hotspots
- mutation operators for prompts/policies/skills
- tournament evaluation on fixed benchmark set
- provenance-tracked winner selection and policy-gated promotion

Exit criteria:

- daily ranked self-improvement backlog generated automatically
- at least one optimizer-selected candidate promoted with no benchmark regression

### Phase 5: Controlled Autonomy Ramp

Goal: raise autonomy thresholds only when data justifies it.

Deliverables:

- risk-tier readiness checks wired to rolling KPI windows
- automatic rollback of autonomy level on sustained degradation
- explicit autonomy change log with rationale and evidence

Exit criteria:

- low-risk lane consistently above mission thresholds for 4 consecutive weeks
- medium-risk lane remains PR-only with high verification pass rate and low rollback trend

## Governance and Safety

- benchmark smoke runs are health checks, not quality proof
- real gate and task-level acceptance checks are required for promotion decisions
- do not bypass guardrails for speed
- any autonomy increase requires measured evidence over time windows, not one-off runs

## Next Execution Sequence

1. Harden supervised self-build loop orchestration and isolated worktree lifecycle.
2. Implement candidate mutation and tournament selection loop.
3. Turn on gradual autonomy-ramp controller based on KPI trends.
