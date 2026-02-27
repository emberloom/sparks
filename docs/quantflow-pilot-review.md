# QuantFlow Pilot Feature Review for Athena

> Source: [pilot.quantflow.studio](https://pilot.quantflow.studio)
> Date: 2026-02-27

Pilot is an autonomous AI development pipeline that converts tickets into PRs.
This document compares its feature set against Athena to identify implementation gaps.

## Feature Gap Analysis

| Pilot Feature | Athena Status | Priority |
|---|---|---|
| Context intelligence (92% token reduction) | **Mature** — `compress_history()` at 80% utilization | Skip |
| Git worktree isolation | **Mature** — used in self-build | Skip |
| Self-review before PR | **Mature** — verify phase in CodeStrategy | Skip |
| Model routing (trivial→fast, complex→deep) | **Partial** — basic classify only | **High** |
| Session resumption (40% token savings) | **Partial** — data stored, no resume | **High** |
| Cost tracking dashboard | **Partial** — token counts exist, no cost calc | **Medium** |
| Autopilot CI loop with auto-merge | **Minimal** — `gh` CLI only, no polling | **High** |
| Ticket/issue polling | **Minimal** — scheduler exists, no issue poll | **High** |
| Hot-upgrade (zero-downtime) | **Minimal** — dynamic tools only | **Low** |
| Dashboard TUI | **None** | **Low** |

---

## Gaps to Implement

### 1. CI Loop Integration — HIGH

Athena can verify locally but cannot poll CI results and iterate on failures.

**Gap:**
- No CI status polling after push
- No automated "read CI logs → diagnose → fix → re-push" loop
- No auto-merge on CI success

**Proposal:**
- Post-verify phase in `strategy/code.rs`: push branch, create PR, poll `gh pr checks`
- On CI failure: read logs via `gh run view --log-failed`, diagnose, fix, re-push
- On CI success + low-risk tier: auto-merge via `gh pr merge`
- Config: `[ci]` with `poll_interval_secs`, `max_retries`, `auto_merge`

**Files:** `src/strategy/code.rs`, `src/config.rs`, `config.example.toml`

---

### 2. Ticket/Issue Polling — HIGH

Athena has no mechanism to automatically discover and claim work from GitHub issues.

**Gap:**
- No background issue polling loop
- No issue claiming/assignment logic
- No complexity evaluation for incoming tickets
- No dispatch-from-issue pipeline

**Proposal:**
- New proactive loop in `src/proactive.rs` polling `gh issue list --label athena`
- Classify issue complexity, respect risk tiers
- Claim by self-assigning + commenting "Athena is working on this"
- Dispatch via existing `manager.rs` task dispatch

**Files:** `src/proactive.rs`, `src/manager.rs`, `src/config.rs`, `config.example.toml`

---

### 3. Intelligent Model Routing — HIGH

Athena classifies tasks (SIMPLE/COMPLEX/DIRECT) but doesn't route subtasks to different models.

**Gap:**
- No per-subtask model selection
- Internal operations (commit messages, summaries) use the same expensive model as code generation
- No cost-aware routing

**Proposal:**
- Extend `classify()` output to include model tier: `fast`, `standard`, `deep`
- Config: `[llm.routing]` with `fast_model`, `standard_model`, `deep_model`
- Route commit messages, file summaries, simple Q&A → fast model
- Route code gen, multi-file changes, complex reasoning → deep model

**Files:** `src/manager.rs`, `src/llm.rs`, `src/config.rs`, `config.example.toml`

---

### 4. Session Resumption — MEDIUM-HIGH

Athena stores conversation history but cannot resume interrupted multi-step tasks.

**Gap:**
- No checkpoint persistence for task state (phase, completed steps, diffs)
- No resume command or crash recovery
- Re-runs exploration from scratch on every attempt

**Proposal:**
- Checkpoint table in SQLite: task_id, phase, step, context_summary, file_diffs
- `athena resume --task-id <id>` CLI command
- Crash recovery: detect incomplete tasks on startup, offer to resume
- Rebuild context from checkpoints instead of re-exploring

**Files:** `src/manager.rs`, `src/db.rs`, `src/main.rs`, `src/core.rs`

---

### 5. Cost Tracking & Analytics — MEDIUM

Athena counts tokens and tracks latency but doesn't calculate actual USD cost.

**Gap:**
- No price-per-model table
- No cost calculation per LLM call
- No cost-per-task or cost-per-lane KPI
- No cost ceiling/budget enforcement

**Proposal:**
- Price table in `llm.rs` (input/output price per 1K tokens per model)
- Compute cost on every LLM call, accumulate per task
- New KPI fields: `total_cost_usd`, `cost_per_task`, `cost_per_lane`
- Surface in `kpi snapshot` and Langfuse events
- Optional `max_cost_per_task` config

**Files:** `src/llm.rs`, `src/kpi.rs`, `src/introspect.rs`, `config.example.toml`

---

## Not Recommended

| Feature | Reason |
|---|---|
| **Hot-upgrade** | Unsafe for a Rust binary. Self-build worktree + restart is sufficient. |
| **Dashboard TUI** | Observer + Langfuse cover monitoring. Revisit if operational complexity grows. |

## Implementation Order

1. **Model Routing** — lowest effort, immediate cost savings
2. **CI Loop** — closes biggest autonomous workflow gap
3. **Issue Polling** — makes Athena fully autonomous end-to-end
4. **Cost Tracking** — enables data-driven routing
5. **Session Resumption** — largest effort, biggest architectural change
