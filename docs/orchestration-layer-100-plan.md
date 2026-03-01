# Orchestration Layer → 100% Plan

Goal: close every gap identified in the 6-area analysis so Athena's orchestration
layer is fully wired and demonstrably working.

---

## Area 1 — Close the Feedback Loop (current: ~60%)

### What's missing

1. **Classifier doesn't query outcomes.** `manager.rs` classify_task uses a pure LLM
   prompt. It never queries `autonomous_task_outcomes` or `kpi_snapshots` for
   historical signal (e.g. "tasks like this in repo X with ghost Y succeed 90%").
2. **Proactive refactoring scanner has no failure suppression.** If Athena suggests
   a refactoring, it fails, and 6 hours later the scanner suggests the same thing.
   No check for "I proposed this before and it failed."
3. **Lessons stored but never retrieved for routing.** `manager.rs:1076` stores
   `lesson` memories after ghost execution but the classifier prompt never includes
   relevant lessons.

### Tasks

#### T1.1 — Inject KPI context into classifier prompt (45 min)
**File:** `src/manager.rs` — classify_task function

Add a `build_kpi_context()` helper that:
- Opens KPI connection via `kpi::open_connection(config)`
- Calls `kpi::compute_snapshot(conn, lane, repo, risk_tier)` for the task's lane/repo/risk
- Formats a 3-line summary: "Recent stats for {repo}/{lane}/{risk}: success_rate={X},
  verification_pass={Y}, rollback_rate={Z}, tasks_started={N}"
- Injects this into the classifier system prompt before the LLM call

**Acceptance criteria:**
- [ ] `build_kpi_context()` returns formatted string with latest KPI for the task's lane/repo/risk
- [ ] Classifier system prompt includes KPI context when DB has data
- [ ] Graceful fallback (empty string) when no KPI data exists
- [ ] `cargo test` passes
- [ ] `cargo check` passes

#### T1.2 — Inject recent lessons into classifier prompt (30 min)
**File:** `src/manager.rs` — classify_task function

Add a `build_lesson_context()` helper that:
- Calls `memory.search("lesson")` with limit 5
- Formats as bullet list: "Recent lessons:\n- {lesson1}\n- {lesson2}"
- Appends to classifier system prompt after KPI context

**Acceptance criteria:**
- [ ] `build_lesson_context()` returns formatted string with up to 5 recent lessons
- [ ] Classifier system prompt includes lessons when memory has data
- [ ] Returns empty string when no lessons exist
- [ ] `cargo test` passes

#### T1.3 — Add failure suppression to proactive refactoring scanner (45 min)
**File:** `src/proactive.rs` — refactoring scanner section

Before dispatching a refactoring suggestion:
- Search memory for `code_change_failed` category containing the same file/pattern
- If found within last 48 hours, skip and log "suppressed: previous attempt failed"
- Store a `refactor_suggestion` memory when dispatching so future checks can match

**Acceptance criteria:**
- [ ] Refactoring scanner checks `code_change_failed` memories before dispatch
- [ ] Suggestions matching a failed attempt in last 48h are suppressed with log message
- [ ] New suggestions store `refactor_suggestion` memory for future matching
- [ ] `cargo test` passes
- [ ] Unit test: mock memory with a failed refactoring → verify dispatch is suppressed

#### T1.4 — Feed self-heal outcomes back into memory (30 min)
**File:** `src/self_heal.rs`, `src/strategy/code.rs`

After a self-heal attempt completes (success or failure):
- Store a `self_heal_outcome` memory with: error_category, fix_attempted, success (bool)
- Before attempting a fix, check if same error_category has a recent successful fix pattern

**Acceptance criteria:**
- [ ] Self-heal stores `self_heal_outcome` memory after each attempt
- [ ] Before fixing, checks for prior successful patterns for same error category
- [ ] `cargo test` passes

### Verification: Area 1 is 100%

1. Run `athena dispatch --goal "add a unit test" --lane delivery --risk low` twice.
   Check classifier prompt in Langfuse trace — must contain KPI summary and lessons.
2. Dispatch a refactoring task that fails. Wait 6h (or mock time). Verify the same
   refactoring is NOT re-proposed (check observer logs for "suppressed").
3. Trigger a self-heal cycle. Verify `self_heal_outcome` memory appears in DB.
4. Run `cargo test` — all existing + new tests pass.

---

## Area 2 — Multi-Task Coordination (current: ~40%)

### What's missing

1. **Cross-task context sharing.** In feature contract DAG dispatch, Task B that
   depends on Task A cannot see Task A's output. Each ghost runs isolated.
2. **Coordinated feature-level rollback.** If task 3/5 fails, tasks 1-2 are not
   reverted. Only the failing task rolls back.
3. **Resource-aware scheduling.** Batch concurrency is hardcoded via
   `ATHENA_FEATURE_BATCH_CONCURRENCY` (default 2). No dynamic adjustment.

### Tasks

#### T2.1 — Pass predecessor task results as context in DAG dispatch (60 min)
**File:** `src/main.rs` — feature dispatch section (around line 3400-3500)

In the feature dispatch loop, after a batch completes:
- Collect stdout/result summaries from completed tasks
- When building the next batch's `AutonomousTask`, append predecessor results to
  the `context` field: "Previous task results:\n- T1 (succeeded): {summary}\n- T2 (succeeded): {summary}"
- Limit to 500 chars per predecessor to avoid context bloat

**Acceptance criteria:**
- [ ] Successor tasks receive predecessor summaries in their `context` field
- [ ] Summaries truncated to 500 chars per predecessor
- [ ] Only succeeded predecessor results are included (not failed/skipped)
- [ ] Feature contract example dispatches correctly with context passing
- [ ] `cargo test` passes

#### T2.2 — Add coordinated feature-level rollback (60 min)
**File:** `src/main.rs` — feature dispatch section

Add a `--rollback-on-failure` flag to `feature dispatch`:
- Track all commits made by succeeded tasks (store commit SHAs)
- If any task fails and the flag is set, revert all tracked commits in reverse order
  via `git revert --no-edit <sha>` on the feature branch
- Log each revert in the feature ledger
- Add `rollback_commits` field to the feature dispatch ledger JSON

**Acceptance criteria:**
- [ ] `--rollback-on-failure` flag accepted by `feature dispatch`
- [ ] On task failure with flag set, all prior task commits are reverted
- [ ] Reverts are logged in the feature ledger with SHAs
- [ ] Without the flag, behavior is unchanged (no regression)
- [ ] `cargo test` passes

#### T2.3 — Dynamic batch concurrency from system metrics (45 min)
**File:** `src/main.rs` — feature dispatch section, `src/introspect.rs`

Replace static `ATHENA_FEATURE_BATCH_CONCURRENCY` with dynamic calculation:
- Read current `SystemMetrics` (RSS, active containers)
- If RSS > 80% of system memory or active_containers > 4, concurrency = 1
- If RSS > 60%, concurrency = min(2, configured)
- Otherwise, use configured value (default 2, max 4)
- Log chosen concurrency at dispatch start

**Acceptance criteria:**
- [ ] Concurrency adjusts based on current system metrics
- [ ] Falls back to configured default when metrics unavailable
- [ ] Observer log shows "batch concurrency={N} (dynamic)" at dispatch start
- [ ] `cargo test` passes

### Verification: Area 2 is 100%

1. Create a 3-task feature contract where T2 depends on T1, T3 depends on T2.
   Dispatch and verify T2's context includes T1's result summary (check Langfuse trace).
2. Create a 3-task contract where T3 is designed to fail. Run with
   `--rollback-on-failure`. Verify T1 and T2 commits are reverted. Run without flag,
   verify commits remain.
3. Run `feature dispatch` while monitoring observer logs. Verify
   "batch concurrency=N (dynamic)" appears.
4. `cargo test` passes.

---

## Area 3 — CI Bridge (current: ~85%)

### What's missing

1. **CI monitor not default-on when promote_mode=auto.** You must explicitly pass
   `--monitor-ci`. Auto-promote should imply CI monitoring.
2. **No post-merge CI monitoring.** After merge, Athena doesn't watch if the merge
   broke main. No automatic revert.

### Tasks

#### T3.1 — Default --monitor-ci when promote_mode=auto (30 min)
**File:** `src/main.rs` — self-build run handler

In the self-build run handler, after parsing args:
- If `promote_mode == "auto"` and `monitor_ci` is not explicitly set, default it to `true`
- Log "auto-enabling CI monitor for promote_mode=auto"

**Acceptance criteria:**
- [ ] `self-build run --promote-mode auto` enables CI monitoring without `--monitor-ci`
- [ ] `self-build run --promote-mode pr` does NOT auto-enable CI monitoring
- [ ] Explicit `--no-monitor-ci` still overrides the default
- [ ] `cargo test` passes

#### T3.2 — Post-merge CI health check (60 min)
**File:** `src/ci_monitor.rs`

Add `monitor_post_merge()` function:
- After successful merge in `try_auto_merge()`, continue polling CI for the merge commit
- Use `gh api repos/{owner}/{repo}/commits/{sha}/check-runs` to check main branch CI
- If CI fails within 10 minutes post-merge, create a revert PR:
  `gh pr create --title "revert: {original_pr_title}" --body "Auto-revert: CI failed post-merge"`
- Add `post_merge_status` and `revert_pr_url` fields to `CiMonitorReport`

**Acceptance criteria:**
- [ ] After auto-merge, polls main CI for 10 minutes
- [ ] If main CI fails, creates revert PR automatically
- [ ] If main CI passes, logs success and exits
- [ ] `CiMonitorReport` includes `post_merge_status` field
- [ ] `cargo test` passes

#### T3.3 — Wire CI monitor into ticket intake auto-dispatch (45 min)
**File:** `src/ticket_intake/sync.rs`, `src/core.rs`

When ticket intake dispatches a task that results in a PR:
- After PR is created and synced back to Linear, chain CI monitor
- Store CI monitor result in the writeback ledger
- Post CI status as a follow-up comment on the Linear issue

**Acceptance criteria:**
- [ ] Ticket-originated PRs trigger CI monitoring automatically
- [ ] CI status appears as Linear comment (e.g. "CI passed, PR merged" or "CI failed, heal attempted")
- [ ] Writeback ledger includes `ci_monitor_status` field
- [ ] `cargo test` passes

### Verification: Area 3 is 100%

1. Run `self-build run --promote-mode auto` (no `--monitor-ci` flag). Verify CI
   monitoring activates (check observer logs for "ci monitor started").
2. After a successful auto-merge, verify post-merge CI polling in observer logs.
3. Create a ticket in Linear with `athena` label that produces a PR. Verify CI
   status comment appears on the Linear issue.
4. `cargo test` passes.

---

## Area 4 — Smarter Classification and Routing (current: ~10%)

### What's missing

1. **No KPI-informed routing.** Classifier doesn't know which ghost/CLI tool
   historically performs best for a given repo/lane/risk combination.
2. **No token budget pre-estimation.** Tasks that will blow the context window
   aren't detected before dispatch.
3. **No CLI tool performance-based routing.** eval_cli_matrix.py compares tools
   but results don't feed into runtime selection.

### Tasks

#### T4.1 — Add ghost success rate to classifier context (45 min)
**File:** `src/manager.rs`, `src/kpi.rs`

Add `query_ghost_success_rates(conn, repo, lane)` to `kpi.rs`:
- Query `autonomous_task_outcomes` grouped by ghost
- Return Vec<(ghost_name, success_rate, task_count)>
- Filter to ghosts with >= 3 tasks for statistical significance

In `manager.rs`, inject ghost stats into classifier prompt:
- "Ghost performance for {repo}/{lane}: coder=85% (20 tasks), scout=92% (12 tasks)"

**Acceptance criteria:**
- [ ] `query_ghost_success_rates()` returns correct rates from DB
- [ ] Classifier prompt includes ghost performance when data exists
- [ ] Ghosts with < 3 tasks are excluded
- [ ] Unit test with mock DB data verifies correct rates
- [ ] `cargo test` passes

#### T4.2 — Add CLI tool success rate tracking and routing (60 min)
**File:** `src/kpi.rs`, `src/strategy/code.rs`, `src/manager.rs`

Track which CLI tool was used per task:
- Add `cli_tool` column to `autonomous_task_outcomes` table (nullable, for backwards compat)
- In code strategy, record which tool was used when storing outcome
- Add `query_cli_tool_success_rates(conn, repo)` to `kpi.rs`
- Inject into classifier: "CLI tool performance: claude_code=88% (15 tasks), codex=72% (8 tasks)"

**Acceptance criteria:**
- [ ] `cli_tool` column added to `autonomous_task_outcomes` with migration
- [ ] Code strategy records cli_tool in outcome
- [ ] `query_cli_tool_success_rates()` returns correct rates
- [ ] Classifier prompt includes CLI tool stats when available
- [ ] `cargo test` passes

#### T4.3 — Token budget pre-estimation (45 min)
**File:** `src/manager.rs`, `src/kpi.rs`

Add `estimate_token_budget(conn, goal_keywords, repo)` to `kpi.rs`:
- Query average token usage for similar past tasks (keyword match on goal)
- Return estimated tokens needed
- If estimate exceeds 80% of context window, add warning to classifier prompt:
  "WARNING: similar tasks averaged {N} tokens, approaching context limit. Consider
  splitting or using a model with larger context."

**Acceptance criteria:**
- [ ] `estimate_token_budget()` queries past task token usage
- [ ] Warning injected into classifier prompt when estimate is high
- [ ] Graceful handling when no historical data exists
- [ ] `cargo test` passes

### Verification: Area 4 is 100%

1. Run 5+ tasks with different ghosts and CLI tools. Then dispatch a new task.
   Check Langfuse trace — classifier prompt must include ghost success rates and
   CLI tool stats.
2. Dispatch a task known to be large. Verify token budget warning appears in
   classifier prompt.
3. Run `cargo test` — all tests pass.
4. Compare routing decisions before/after: new dispatches should prefer
   historically successful ghost/tool combinations (inspect Langfuse traces).

---

## Area 5 — Self-Improvement Loop (current: ~35%)

### What's missing

1. **Tournament only mutates system prompts.** No mutation of tool usage patterns,
   constraints, soul files, or ghost configurations.
2. **No automatic ghost specialization.** Athena can't create tuned ghost profiles
   from observed patterns.
3. **No policy/guardrail evolution.** Guardrails are static — can't tighten or
   relax based on safety record.

### Tasks

#### T5.1 — Add constraint and soul file mutation axes to tournament (60 min)
**File:** `scripts/optimizer_tournament.py`

Extend `generate_mutations()`:
- **Constraint mutations:** add/remove/modify constraint lines in task contracts
  (e.g. "Always run cargo fmt after changes", "Limit changes to 3 files")
- **Soul file mutations:** vary ghost personality parameters (verbosity, caution level,
  preferred language patterns)
- Track mutation axis in tournament results for attribution

**Acceptance criteria:**
- [ ] Tournament generates constraint mutations alongside system prompt mutations
- [ ] Tournament generates soul file mutations
- [ ] Each mutation is tagged with its axis (prompt/constraint/soul)
- [ ] Tournament results report which axis produced the best candidates
- [ ] `python3 scripts/test_optimizer_tournament.py` passes

#### T5.2 — Auto-generate specialized ghost profiles from outcomes (60 min)
**File:** `src/proactive.rs`, new: `src/ghost_specializer.rs`

Add a ghost specialization scanner to the proactive loop (runs every 24h):
- Query `autonomous_task_outcomes` grouped by repo + ghost
- If a repo has > 10 tasks and one ghost has > 20% better success rate, propose
  a specialized ghost config:
  - Copy base ghost config
  - Add repo-specific constraints from successful task patterns
  - Write to `~/.athena/ghosts/{repo}-specialist.toml`
- Require spontaneity gate before writing (Level 4 autonomy)

**Acceptance criteria:**
- [ ] Specialization scanner runs on 24h schedule
- [ ] Generates ghost profile when statistical threshold met (>10 tasks, >20% delta)
- [ ] Written profile includes repo-specific constraints
- [ ] Gated by spontaneity knob
- [ ] `cargo test` passes

#### T5.3 — Adaptive guardrail relaxation based on safety record (45 min)
**File:** `src/tools.rs` — tool allowlist section, `src/kpi.rs`

Add `compute_safety_record(conn, ghost, repo)` to `kpi.rs`:
- Count critical safety incidents (guardrail violations) in last 30 days
- If zero incidents over > 50 tasks, mark as "trusted"

In `tools.rs`, when building tool allowlist for a ghost:
- If ghost+repo is "trusted", expand allowlist with one additional tool tier
  (e.g. scout gets shell_write, coder gets network access)
- Log "elevated trust: {ghost}+{repo} — 0 incidents in {N} tasks"

**Acceptance criteria:**
- [ ] `compute_safety_record()` returns incident count and task count
- [ ] "Trusted" status requires 0 incidents over > 50 tasks
- [ ] Trusted ghosts get expanded tool allowlist (one tier up)
- [ ] Expansion is logged via observer
- [ ] Any new incident immediately revokes trusted status
- [ ] `cargo test` passes

### Verification: Area 5 is 100%

1. Run optimizer tournament with `--mutation-axes prompt,constraint,soul`.
   Verify all three axes appear in results report.
2. Populate `autonomous_task_outcomes` with 15+ entries where coder ghost
   outperforms scout on repo X. Run proactive loop and verify
   `~/.athena/ghosts/X-specialist.toml` is created.
3. Populate 60 clean tasks (0 guardrail violations) for coder+athena.
   Verify observer shows "elevated trust" on next dispatch.
4. `cargo test` passes.

---

## Area 6 — Observability as a Product (current: ~25%)

### What's missing

1. **No visual dashboard.** CLI markdown only.
2. **No token cost aggregation.** Tracked in Langfuse but not surfaced.
3. **No trend visualization.** KPI snapshots stored but no historical view.
4. **No per-ghost performance comparison.** Ghost column exists but isn't surfaced.

### Tasks

#### T6.1 — Add KPI trend rendering to eval_dashboard.py (45 min)
**File:** `scripts/eval_dashboard.py`

Add `render_trends()` function:
- Query `kpi_snapshots` ordered by `captured_at` for last 30 days
- Render ASCII sparklines for task_success_rate, verification_pass_rate, rollback_rate
- Show direction arrows (↑↓→) for 7-day trend vs 30-day average

**Acceptance criteria:**
- [ ] Dashboard renders trend sparklines for 3 core KPI metrics
- [ ] Shows directional arrows for 7d vs 30d comparison
- [ ] Handles empty data gracefully (shows "No trend data")
- [ ] `python3 scripts/test_eval_harness.py` passes (add trend tests)

#### T6.2 — Add per-ghost performance breakdown (30 min)
**File:** `scripts/eval_dashboard.py`, `src/kpi.rs`

Add `query_ghost_performance(conn)` to `kpi.rs`:
- Group `autonomous_task_outcomes` by ghost, compute success_rate, avg duration, task count
- Return sorted by task count descending

Add `render_ghost_performance()` to dashboard:
- Table: Ghost | Tasks | Success% | Avg Duration | Rollback%

**Acceptance criteria:**
- [ ] `query_ghost_performance()` returns per-ghost metrics
- [ ] Dashboard renders ghost performance table
- [ ] Sorted by task count descending
- [ ] `cargo test` passes

#### T6.3 — Add token cost tracking and display (45 min)
**File:** `src/kpi.rs`, `src/core.rs`, `scripts/eval_dashboard.py`

Track token usage per task:
- Add `tokens_used` column to `autonomous_task_outcomes` (nullable integer)
- In `handle_autonomous_task_success/failure`, record token count from strategy result
- Add `render_cost_summary()` to dashboard:
  - Total tokens last 7d / 30d
  - Avg tokens per task by lane
  - Estimated cost at $X/1M tokens (configurable)

**Acceptance criteria:**
- [ ] `tokens_used` column added with migration
- [ ] Token count recorded per task outcome
- [ ] Dashboard renders cost summary with configurable $/1M rate
- [ ] `cargo test` passes

#### T6.4 — HTML dashboard output mode (60 min)
**File:** `scripts/eval_dashboard.py`

Add `--format html` flag:
- Generate self-contained HTML file with:
  - KPI summary cards (success rate, verification rate, rollback rate)
  - Trend charts using inline SVG (no JS dependencies)
  - Ghost performance table
  - Cost summary
  - Recent task history (last 20 tasks)
- Write to `eval/results/dashboard.html`

**Acceptance criteria:**
- [ ] `--format html` produces valid HTML file
- [ ] HTML is self-contained (no external dependencies)
- [ ] All sections render correctly (KPI cards, trends, ghost table, costs)
- [ ] Opens correctly in browser
- [ ] `--format markdown` (default) behavior unchanged

### Verification: Area 6 is 100%

1. Run `eval_dashboard.py` — verify trend sparklines appear.
2. Run `eval_dashboard.py` — verify ghost performance table appears.
3. After running 5+ tasks, run dashboard — verify cost summary with token counts.
4. Run `eval_dashboard.py --format html` — open in browser, verify all sections render.
5. All tests pass.

---

## Task Summary

| ID | Area | Task | Est. Time | Dependencies |
|----|------|------|-----------|--------------|
| T1.1 | Feedback Loop | KPI context in classifier | 45 min | — |
| T1.2 | Feedback Loop | Lessons in classifier | 30 min | — |
| T1.3 | Feedback Loop | Refactoring suppression | 45 min | — |
| T1.4 | Feedback Loop | Self-heal outcome memory | 30 min | — |
| T2.1 | Multi-Task | Cross-task context passing | 60 min | — |
| T2.2 | Multi-Task | Coordinated rollback | 60 min | — |
| T2.3 | Multi-Task | Dynamic batch concurrency | 45 min | — |
| T3.1 | CI Bridge | Default CI monitor for auto | 30 min | — |
| T3.2 | CI Bridge | Post-merge CI check | 60 min | T3.1 |
| T3.3 | CI Bridge | CI monitor in ticket intake | 45 min | T3.2 |
| T4.1 | Classification | Ghost success rates in routing | 45 min | T1.1 |
| T4.2 | Classification | CLI tool tracking + routing | 60 min | T4.1 |
| T4.3 | Classification | Token budget pre-estimation | 45 min | — |
| T5.1 | Self-Improvement | Tournament mutation axes | 60 min | — |
| T5.2 | Self-Improvement | Auto ghost specialization | 60 min | T4.1 |
| T5.3 | Self-Improvement | Adaptive guardrail relaxation | 45 min | T4.1 |
| T6.1 | Observability | KPI trend rendering | 45 min | — |
| T6.2 | Observability | Ghost performance breakdown | 30 min | — |
| T6.3 | Observability | Token cost tracking | 45 min | — |
| T6.4 | Observability | HTML dashboard | 60 min | T6.1, T6.2, T6.3 |

**Total: 20 tasks, ~15.5 hours**

### Recommended execution order (parallelizable groups)

**Batch 1 (no dependencies):** T1.1, T1.2, T1.3, T1.4, T2.1, T2.3, T3.1, T4.3, T5.1, T6.1, T6.2, T6.3
**Batch 2 (depends on Batch 1):** T2.2, T3.2, T4.1, T4.2
**Batch 3 (depends on Batch 2):** T3.3, T5.2, T5.3, T6.4

---

## End-to-End Verification Protocol

After all tasks complete, run this checklist:

1. **Smoke test:** `cargo check && cargo test && cargo test --features telegram`
2. **Maintainability:** `python3 scripts/maintainability_check.py`
3. **CI harness:** `make eval-smoke` (mock dispatch smoke)
4. **User flow:** `make user-flow` (Linear mock end-to-end)
5. **KPI verification:** Dispatch 10 tasks across 2 repos, 2 lanes, 2 risk tiers.
   Run `athena kpi snapshot` for each combination. Verify all fields populated.
6. **Classifier verification:** Dispatch task after KPI population. Inspect
   Langfuse trace for classifier prompt — must contain: KPI stats, lesson context,
   ghost success rates, CLI tool stats.
7. **Feature contract verification:** Run 3-task DAG with `--rollback-on-failure`.
   Verify cross-task context and rollback behavior.
8. **CI bridge verification:** Run `self-build run --promote-mode auto`. Verify
   auto-CI-monitor, post-merge check, and ticket writeback.
9. **Self-improvement verification:** Run optimizer tournament with 3 mutation axes.
   Check ghost specialization after sufficient data.
10. **Dashboard verification:** Run `eval_dashboard.py --format html`. Open in
    browser. Verify all sections (trends, ghosts, costs, history).
