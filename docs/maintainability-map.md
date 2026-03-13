# Emberloom Maintainability Map

Date: 2026-02-15
Scope: `src/**/*.rs` (31 Rust files)

## 1) Baseline Metrics

- Total Rust LOC: `18,137`
- Total functions: `599`
- Functions over 80 lines: `36`
- Functions over 120 lines: `24`
- Largest files:
  - `src/tools.rs` (2352)
  - `src/llm.rs` (1559)
  - `src/main.rs` (1469)
  - `src/telegram.rs` (1155)
  - `src/memory.rs` (1109)
  - `src/config.rs` (1073)
  - `src/strategy/code.rs` (998)
  - `src/proactive.rs` (917)
  - `src/manager.rs` (896)
  - `src/dynamic_tools.rs` (880)

## 2) Hotspot Map (Production Code)

Largest production functions by size:

1. `src/main.rs:450` `run_funnel_health` (~626)
2. `src/telegram.rs:290` `handle_message` (~609)
3. `src/core.rs:167` `start` (~512)
4. `src/main.rs:1188` `run_chat` (~282)
5. `src/knobs.rs:139` `set` (~240)
6. `src/manager.rs:156` `handle` (~237)
7. `src/llm.rs:900` `chat_with_tools_stream` (~212)
8. `src/manager.rs:483` `classify` (~205)
9. `src/strategy/react.rs:43` `run_native` (~197)
10. `src/proactive.rs:412` `maybe_schedule_reentry` (~192)
11. `src/proactive.rs:42` `spawn_memory_scanner` (~185)
12. `src/introspect.rs:161` `spawn_metrics_collector` (~182)
13. `src/proactive.rs:229` `spawn_idle_musings` (~179)
14. `src/strategy/code.rs:377` `execute_code` (~174)
15. `src/proactive.rs:735` `spawn_refactoring_scanner` (~171)

Interpretation:

- This codebase has a few orchestration-heavy "mega functions" that dominate maintenance risk.
- Risk is concentrated in chat/control-plane, proactive loops, manager orchestration, and strategy execution phases.

## 3) Module Coupling Map

Highest fan-out modules (most internal dependencies):

- `src/core.rs`: 19 deps
- `src/manager.rs`: 15 deps
- `src/executor.rs`: 12 deps
- `src/strategy/code.rs`, `src/strategy/mod.rs`, `src/strategy/react.rs`: 8 deps each
- `src/heartbeat.rs`, `src/proactive.rs`: 8 deps each

High fan-in shared modules (widely depended upon):

- `src/error.rs` (19 inbound)
- `src/llm.rs` (11 inbound)
- `src/langfuse.rs` (10 inbound)
- `src/config.rs` (9 inbound)
- `src/observer.rs` (9 inbound)
- `src/knobs.rs` (8 inbound)

Interpretation:

- `core` and `manager` are acting as architectural hubs (expected), but complexity in these hubs is now too high.
- `llm`, `config`, and `tools` are shared centers; changes there have broad blast radius.

## 4) Testing Distribution Map

Files with notable tests:

- `src/tools.rs` (55)
- `src/llm.rs` (24)
- `src/dynamic_tools.rs` (24)
- `src/memory.rs` (21)

Sparse/no tests in high-risk orchestration code:

- `src/main.rs` (0)
- `src/core.rs` (0)
- `src/manager.rs` (0)
- `src/proactive.rs` (0)
- `src/telegram.rs` (0)
- `src/strategy/react.rs` (0)
- `src/executor.rs` (0)

Interpretation:

- Utility/tool layers are tested.
- Runtime orchestration loops and funnel glue are under-tested relative to their complexity.

## 5) Funnel-to-Code Ownership Map

### Funnel 1: Health Monitor -> Diagnose -> Auto-Fix

- Primary files:
  - `src/introspect.rs` (metrics collection/anomaly trigger)
  - `src/proactive.rs` (background loops and dispatch gating)
  - `src/self_heal.rs` + `src/executor.rs` + `src/strategy/code.rs` (self-heal execution path)
  - `src/main.rs` (`doctor` diagnostics)

Maintainability risk:

- Logic spans many modules with limited contract tests.

### Funnel 2: Index -> Analyze -> Propose -> Refactor

- Primary files:
  - `src/proactive.rs` (index/refactor scanners)
  - `src/manager.rs` and `src/strategy/code.rs` (task execution)
  - `src/memory.rs` (artifact storage)

Maintainability risk:

- Scanner logic and dispatch policy are large, stateful, and currently difficult to unit test in isolation.

### Funnel 3: Interact -> Learn -> Evolve

- Primary files:
  - `src/telegram.rs`, `src/main.rs` chat loop
  - `src/heartbeat.rs`, `src/proactive.rs`
  - `src/memory.rs`

Maintainability risk:

- User interaction surface (`telegram.rs`) is monolithic; command handling and message processing are tightly coupled.

### Funnel 4: Execute -> Verify -> Self-Heal -> Learn

- Primary files:
  - `src/strategy/code.rs`, `src/strategy/react.rs`, `src/strategy/mod.rs`
  - `src/executor.rs`, `src/tools.rs`, `src/docker.rs`
  - `src/self_heal.rs`

Maintainability risk:

- Multiple strategy paths (native/text fallback) with overlapping logic increase divergence risk.

## 6) Maintainability Thresholds (Recommended)

- Function hard cap: `120` lines (exceptions require explicit comment/rationale)
- Function target: `<= 80` lines
- File soft cap: `800` LOC
- File hard attention threshold: `1000` LOC
- Internal module fan-out attention threshold: `> 10` deps
- Any function with both:
  - `> 120` lines, and
  - complex branching (many `if/match/loop`)
  should be top-priority for decomposition.

## 7) Priority Backlog (Refactor Program)

### P0 (immediate, highest ROI)

1. Split `run_funnel_health` in `src/main.rs:450` into:
   - data collection
   - per-funnel check builders
   - rendering/output
2. Split `handle_message` in `src/telegram.rs:290` by command family:
   - command routing
   - stateful callbacks
   - pure formatters
3. Decompose `core::start` in `src/core.rs:167` into startup phases with clear contracts.

### P1 (next)

1. Break `manager::handle` / `manager::classify` into policy modules:
   - routing policy
   - memory context assembly
   - execution bridging
2. Decompose proactive loops in `src/proactive.rs`:
   - scheduling
   - memory retrieval
   - prompting
   - dispatch gating
3. Separate `llm` stream assembly/parser concerns from provider transport logic.

### P2 (stabilization)

1. Reduce duplication between code/react strategy execution phases.
2. Introduce contract tests per funnel path (not only utility tests).
3. Add maintainability checks in CI (threshold report + fail-on-regression).

## 8) Suggested CI Guardrails

- Add a script producing:
  - file LOC leaderboard
  - functions over 80/120 lines
  - module fan-out report
- CI mode:
  - warn on threshold violations
  - fail only on regression against checked-in baseline (to avoid blocking all work initially)

---

This map is intentionally quantitative and actionable: it shows where complexity is concentrated, which funnels are most fragile, and what refactor order gives the highest payoff first.

## 9) Closure Update (2026-02-26)

Current snapshot after refactor pass:

- Total Rust LOC: `27,013`
- Total functions: `874`
- Functions over 80 lines: `51`
- Functions over 120 lines: `30`
- Current top hotspot function:
  - `src/embeddings.rs::bench_memory_retrieval` (`835` lines)

Validation status:

- `cargo check -q`: pass
- `cargo test -q`: pass
- `ATHENA_DISABLE_HOME_PROFILES=1 cargo run -- doctor --skip-llm --ci`: `WARN` (exit `0`)
- `scripts/maintainability_check.py`: pass

## 10) Maintainer Policy and Next Tranche

### Policy (enforced in CI)

- Maintainability regression gate now focuses on function complexity:
  - `fn_over_80`
  - `fn_over_120`
  - `max_fn_len`
- File-size growth metrics are still reported for visibility, but not failing checks in a dirty/parallel-refactor branch.

### Next Tranche Backlog (>120-line hotspots)

1. `src/manager.rs`
   - Split `handle` and `classify` into routing policy + execution bridge + memory context modules.
2. `src/proactive.rs`
   - Split `spawn_memory_scanner`, `spawn_idle_musings`, `maybe_schedule_reentry`, `spawn_refactoring_scanner` by scheduler/prompting/dispatch concerns.
3. `src/strategy/code.rs`
   - Decompose `execute_code` and `verify_native` around plan-build/execute/verify stages.
4. `src/strategy/react.rs`
   - Split `run_native` into prompt assembly, tool routing, and completion handling.
5. `src/llm.rs`
   - Separate stream assembly/parsing from provider transport.

### Test Backlog for Refactor Safety

1. CLI command routing parser tests (added) should be expanded to include malformed slash-commands.
2. Telegram event formatting/rate-limit tests (added) should be expanded to include chunking boundaries and error rendering.
3. Add module-level tests for manager/proactive refactor seams before further extraction.
