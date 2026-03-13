# Sparks Self-Improvement Roadmap

Date: 2026-02-17

## North Star

Build Sparks into a spec-driven engineering agent that can reliably deliver backlog work across products and improve itself through measured, policy-bounded iteration.

## Current State (Baseline)

As of 2026-02-16 to 2026-02-17:

- strong coding-agent backbone exists across `claude_code`, `codex`, and `opencode`
- autonomous task loop, outcomes, and memory logging exist
- eval harness, matrix runs, history, and dashboard exist
- strict real-gate scoring now exists, including task-level delivery minima

Key gaps versus target:

- no OpenEvolve-style prompt and skill mutation/selection loop
- optimizer tournaments are now implemented, but mutation breadth and multi-day winner stability are still limited
- limited feature-level orchestration from one feature spec into multiple dependent tasks
- full real-gate remains self-hosted rather than universal hosted CI

Maturity estimate:

- agent execution layer: ~85%
- evaluation layer: ~65%
- failure logging/telemetry layer: ~70%
- self-improvement optimizer: ~30%
- end-to-end closed loop (execute -> evaluate -> evolve -> promote): ~55-60%

## Parsing Hardening Status (2026-02-26)

- [ ] Phase 0 — baseline telemetry counters + snapshot artifact
- [~] Phase 1 — CLI contract parsing hardening (marker-anywhere + tests done; shared parser + param-validation markers pending)
- [ ] Phase 2 — classifier contract schema + structured error codes
- [~] Phase 3 — strategy text fallback normalization (strict JSON envelope done; repair turn + reason taxonomy pending)
- [~] Phase 4 — eval harness structured plan scoring (JSON scoring + legacy fallback done; artifact-based scoring pending)
- [ ] Phase 5 — strict parsing rollout + gates

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
  - `sparks feature validate --file <contract>`
  - `sparks feature plan --file <contract>`
  - `sparks feature dispatch --file <contract>`
  - `sparks feature verify --file <contract>`
  - `sparks feature promote --file <contract>`
  - `sparks feature gate --file <contract> --verify-profile <fast|strict>`
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
- gate artifacts: `eval/results/feature-gate-sparks-phase2-closeout-*.json` (`5/5 gate_ok=true`)
- summary report: `eval/results/phase1-phase2-closeout-latest.md`

### Phase 3: Supervised Self-Build Pipeline (Closed 2026-02-17)

Goal: Sparks can improve Sparks in a bounded loop.

Deliverables:

- loop: detect issue -> propose patch -> isolated worktree implementation -> maintenance pack -> critic review -> promote decision
- `sparks self-build run` now emits supervised promotion execution artifacts with explicit mode:
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

Closure evidence:

- closeout report: `eval/results/phase3-closeout-latest.md`
- latest supervised batch baseline: `eval/results/self-build-batch-20260217T182304Z.json` (`3/3 succeeded`)
- latest 20 ledger runs: `guardrail_passed=20/20`
- KPI snapshots (2026-02-17 18:24 UTC):
  - `self_improvement low`: success `92.19%`, verify `100%`, rollback `0.85%`
  - `delivery low`: success `40.30%`, verify `100%`, rollback `0%`

### Phase 4: Optimizer Loop (OpenEvolve-Style)

Goal: continuous prompt, policy, and skill evolution from benchmark feedback.

Deliverables:

- candidate generation from failures and maintainability hotspots
- mutation operators for prompts/policies/skills
- tournament evaluation on fixed benchmark set
- provenance-tracked winner selection and policy-gated promotion
- implemented tournament runner:
  - `scripts/optimizer_tournament.py`
  - emits `eval/results/optimizer-tournament-*.{json,md}` with regression gates and winner selection
  - promotes active profile only when non-regression gates + positive delta hold (`eval/results/optimizer-profile.json`)
  - nightly self-hosted run: `.github/workflows/optimizer-tournament-nightly.yml`

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

## Six-Mistake Check (2026-02-17)

Priority fixes currently focused on:

- #2 complicated solutions:
  - mitigation: keep optimizer/tournament logic in small tested functions (`scripts/test_optimizer_tournament.py`)
  - next: continue decomposition of control-plane hotspots from maintainability map
- #4 fragile parsing:
  - mitigation: tournament now parses key-value outputs robustly, with regression tests for parsing paths
  - next: replace remaining heuristic contracts with structured markers in manager/strategy loops
  - implementation plan: `docs/parsing-hardening-plan.md`
- #6 evals not always-on:
  - mitigation: nightly full real-gate optimizer workflow added on self-hosted runner
  - next: move from manual/self-hosted-only dependence toward broader always-on gating where possible

## Next Execution Sequence

1. Expand mutation operators beyond dispatch-context prompts (policy/tool/skill-level variants).
2. Add rolling-window promotion policy (multi-night consistency before profile promotion).
3. Continue hardening parsing contracts in manager/strategy to remove brittle string heuristics.
