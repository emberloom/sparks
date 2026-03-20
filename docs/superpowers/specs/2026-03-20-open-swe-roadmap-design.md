# Open SWE Feature Adoption Roadmap

**Date:** 2026-03-20
**Source:** LangChain Open SWE (github.com/langchain-ai/open-swe, March 2026)
**Scope:** 5 architectural patterns from Open SWE evaluated for adoption in Sparks

---

## Background

Open SWE captures the architecture that Stripe, Coinbase, and Ramp independently converged on
for internal AI coding agents. Five patterns from that architecture are worth adopting in Sparks.
Each entry below includes a fit analysis, key design decisions, module touchpoints, and effort
estimate. Items are ordered by impact × effort ROI.

---

## 1. Middleware Safety Net

**Priority:** 1 — foundational, enables reliability guarantees for all sparks

### Fit Analysis

Sparks runs tool loops in `executor.rs` and orchestrates sessions in `manager.rs`, but there is
no deterministic post-run guarantee layer. If a spark completes without flushing memory or
writing a KPI snapshot, that operation simply does not happen. A `SparkMiddleware` trait with
two lifecycle points — `before_model_call` and `after_spark_complete` — gives a deterministic
safety net without requiring LLM cooperation.

### Key Design Decisions

- New `SparkMiddleware` trait in `src/middleware.rs` with two async methods:
  `before_model_call(&self, ctx: &SessionContext)` and
  `after_spark_complete(&self, ctx: &SessionContext, outcome: &TaskOutcome)`.
- Implementations registered at startup and stored on the executor.
- Built-in middlewares to ship first: memory flush, KPI snapshot, activity log close.
- `before_model_call` runs inside the `executor.rs` loop before each LLM call.
- `after_spark_complete` runs in `manager.rs` after the task contract resolves — including on
  error paths, so guarantees hold even when a spark panics or times out.
- Per-middleware enable/disable via config to allow staged rollout.

### Module Touchpoints

| File | Change |
|------|--------|
| `src/middleware.rs` | New — trait definition + built-in implementations |
| `src/executor.rs` | Call `before_model_call` before each LLM invocation |
| `src/manager.rs` | Call `after_spark_complete` in post-run cleanup path |
| `src/config.rs` | Add `[middleware]` section with per-middleware toggles |

### Effort

Medium — ~3–5 days. Trait + hook plumbing is straightforward; the effort is in wiring
all error paths in `manager.rs` and implementing the first three built-in middlewares.

---

## 2. Rich Context Injection

**Priority:** 2 — high task quality payoff, largely scaffolded already

### Fit Analysis

`ticket_intake/` already fetches from GitHub, GitLab, Jira, and Linear and produces
`AutonomousTask`. The gap is that only the ticket title and description land in the task
contract — PR diffs, comments, and linked issues are discarded at fetch time.
`context_budget.rs` already handles oversized context trimming, making injection safe to add
without risk of blowing the context window.

### Key Design Decisions

- `TicketProvider` gains a `fetch_full_context(&self, ticket_id) -> TicketContext` method
  returning structured rich text (description, comments, diff summary, linked items).
- Inject as a fenced markdown block prepended to the spark's system prompt at `TaskContract`
  construction time in `manager.rs`.
- Trim order when over budget: comments first, then diff, then description — preserve the
  most critical signal.
- Context injection is opt-in per provider via config (`inject_full_context = true`) so
  operators can disable it for high-volume intake sources.

### Module Touchpoints

| File | Change |
|------|--------|
| `src/ticket_intake/provider.rs` | Add `fetch_full_context()` to `TicketProvider` trait |
| `src/ticket_intake/github.rs` | Implement: fetch PR diff + comments |
| `src/ticket_intake/linear.rs` | Implement: fetch issue body + comments |
| `src/manager.rs` | Assemble and prepend rich context at `TaskContract` build |
| `src/context_budget.rs` | Add trim strategy for injected context blocks |
| `src/strategy.rs` | Add `rich_context: Option<String>` field to `TaskContract` |

### Effort

Low-Medium — ~2–3 days. Mostly wiring existing data through to prompt construction.
GitHub and Linear are the priority providers; Jira and GitLab can follow.

---

## 3. Mid-Run Message Injection

**Priority:** 3 — high UX impact for interactive use across all frontends

### Fit Analysis

All three frontends (Slack, Teams, Telegram) currently ignore or reject messages that arrive
while a spark is running. The fix is a per-session message queue checked by `executor.rs`
before each model call. Messages in the queue become user-role messages injected at the top of
the next turn, letting operators steer a running spark without interrupting it.

### Key Design Decisions

- Queue type: `Arc<Mutex<VecDeque<InjectMessage>>>` where `InjectMessage` holds the text and
  source platform. Lives on `SessionContext` in `core.rs` — frontends already hold a reference.
- Frontend routing logic: if session is active → push to queue; if idle → start new session
  (existing behavior unchanged).
- Inject point in `executor.rs`: drain the queue before each LLM call and prepend as
  user-role messages to the message history.
- `max_queued_messages` config cap (default: 5) to prevent unbounded injection on busy sessions.
- Queue is cleared on spark completion to avoid leaking messages into the next run.

### Module Touchpoints

| File | Change |
|------|--------|
| `src/core.rs` | Add `inject_queue` field to `SessionContext` |
| `src/executor.rs` | Drain queue and prepend messages before each LLM call |
| `src/slack.rs` | Route mid-run messages to queue instead of rejecting |
| `src/teams.rs` | Same as slack |
| `src/telegram.rs` | Same as slack |
| `src/config.rs` | Add `max_queued_messages` to execution config |

### Effort

Medium — ~4–5 days. The queue itself is simple; the effort is in touching three frontend
files and handling edge cases (queue drain on timeout, queue visibility in `/session` output).

---

## 4. Tool Curation Profiles

**Priority:** 4 — quick win that formalizes existing allowlist mechanism

### Fit Analysis

`GhostConfig` already has a `tool_allowlist` field per spark. As the MCP registry grows,
copying the same list across multiple ghost configs becomes error-prone. Named profiles let
operators define a list once and reference it by name, without changing any runtime behavior —
profiles resolve to the existing allowlist at startup.

### Key Design Decisions

- New `[tool_profiles]` config section: a map of `profile_name → Vec<String>` (tool names).
- `GhostConfig.tool_allowlist` accepts either an inline list or a `profile = "name"` reference.
  Inline list takes precedence if both are present.
- `doctor` validates: all referenced profiles exist, all tools in a profile are registered in
  the MCP registry or built-in tool set. Unknown tools emit a warning, not an error, to
  avoid blocking startup on temporarily unavailable MCP servers.
- Ship common profiles in `config.example.toml`: `"researcher"` (web + memory tools),
  `"devops"` (docker + shell + git), `"code-reviewer"` (read-only file + search tools).

### Module Touchpoints

| File | Change |
|------|--------|
| `src/config.rs` | Add `ToolProfiles` type, update `GhostConfig` to accept profile ref |
| `src/doctor.rs` | Validate profile existence and tool registration |
| `config.example.toml` | Add `[tool_profiles]` section with example profiles |

### Effort

Low — ~1–2 days. Pure config schema and validation; no runtime changes.

---

## 5. Per-Spark Todo Lists

**Priority:** 5 — transparency improvement, complements session review

### Fit Analysis

`session_review.rs` tracks what a spark did retrospectively. Todo lists are prospective: the
spark writes what it plans to do at the start of a task, then checks items off. This makes
long-running sparks observable mid-run and gives the `/session` command a live progress view.
Open SWE treats this as a first-class tool (`write_todos`) rather than an external tracker.

### Key Design Decisions

- Two new tools registered per executor session:
  - `todo_write(items: Vec<String>)` — replaces the current list (idempotent, call at task start
    and whenever the plan changes).
  - `todo_check(index: usize)` — marks item at index as done.
- State held in-memory on the executor session struct; persisted to SQLite via `db.rs` on
  session close.
- Surfaced in `/session` output on all frontends as a progress block (e.g. `✓ [done]`,
  `→ [active]`, `○ [pending]`).
- Not required — sparks that never call `todo_write` simply have an empty list; no behavior
  change for existing ghost configs.

### Module Touchpoints

| File | Change |
|------|--------|
| `src/todo.rs` | New — `TodoList` state type + tool handler functions |
| `src/executor.rs` | Register todo tools per session, hold `TodoList` on session state |
| `src/db.rs` | Add `spark_todos` table; persist on session close |
| `src/session_review.rs` | Include todo list in session close payload |
| `src/telegram.rs` / `src/slack.rs` | Render todo block in `/session` output |

### Effort

Medium — ~3–4 days. New tool + persistence + display across frontends.

---

## Summary

| # | Feature | Effort | Builds On |
|---|---------|--------|-----------|
| 1 | Middleware safety net | ~3–5 days | — |
| 2 | Rich context injection | ~2–3 days | `ticket_intake/` |
| 3 | Mid-run message injection | ~4–5 days | All frontends |
| 4 | Tool curation profiles | ~1–2 days | `GhostConfig.tool_allowlist` |
| 5 | Per-spark todo lists | ~3–4 days | `session_review`, `db` |

Total estimated effort: **~13–19 days** across 5 sequential features.
Middleware (#1) should land first as other features can register their own post-run guarantees
against it once it exists.
