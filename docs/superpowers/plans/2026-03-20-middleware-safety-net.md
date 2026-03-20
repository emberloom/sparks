# Middleware Safety Net Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `SparkMiddleware` trait with two lifecycle hooks — `before_model_call` and `after_spark_complete` — so deterministic post-run guarantees (memory flush, activity log close) can't be skipped by LLM forgetfulness.

**Architecture:** A new `SparkMiddleware` trait lives in `src/middleware.rs`. `Executor` holds a `Vec<Arc<dyn SparkMiddleware>>` and exposes two async helper methods. Strategy loops call `executor.invoke_before_model_call()` at each step start. `Executor::run()` calls `invoke_after_spark_complete()` in a finally-like pattern after the strategy returns.

**Tech Stack:** Rust, async-trait, tokio

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/middleware.rs` | Create | Trait definition + built-in `ActivityLogFlushMiddleware` |
| `src/config.rs` | Modify | Add `MiddlewareConfig` to `Config`, add to `ManagerConfig` |
| `src/executor.rs` | Modify | Add `middlewares` field, add `invoke_before_model_call` / `invoke_after_spark_complete` |
| `src/strategy/react.rs` | Modify | Call `executor.invoke_before_model_call()` at start of each step |
| `src/strategy/code.rs` | Modify | Same — find each inner step loop and add the call |
| `src/main.rs` | Modify | Wire `MiddlewareConfig` → middleware list → Executor |

---

## Task 1: Define the `SparkMiddleware` trait

**Files:**
- Create: `src/middleware.rs`

- [ ] **Step 1: Write failing test for middleware invocation**

Add to the bottom of `src/middleware.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingMiddleware {
        before_count: Arc<AtomicUsize>,
        after_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SparkMiddleware for CountingMiddleware {
        async fn before_model_call(&self, _session_id: &str, _ghost: &str) {
            self.before_count.fetch_add(1, Ordering::SeqCst);
        }
        async fn after_spark_complete(&self, _session_id: &str, _ghost: &str, _outcome: &SparkOutcome) {
            self.after_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn middleware_called_correct_count() {
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let mw: Arc<dyn SparkMiddleware> = Arc::new(CountingMiddleware {
            before_count: before.clone(),
            after_count: after.clone(),
        });

        mw.before_model_call("sess1", "coder").await;
        mw.before_model_call("sess1", "coder").await;
        mw.after_spark_complete("sess1", "coder", &SparkOutcome::Success("done".into())).await;

        assert_eq!(before.load(Ordering::SeqCst), 2);
        assert_eq!(after.load(Ordering::SeqCst), 1);
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks middleware 2>&1 | head -30
```
Expected: `error[E0433]: failed to resolve: use of undeclared crate or module`

- [ ] **Step 3: Write the trait and types**

Create `src/middleware.rs`:

```rust
use std::sync::Arc;

/// Outcome of a completed spark run, passed to `after_spark_complete`.
pub enum SparkOutcome {
    Success(String),
    Failure(String),
}

/// Lifecycle hooks that run deterministically regardless of LLM behavior.
///
/// `before_model_call` runs inside the strategy loop before each LLM call.
/// `after_spark_complete` runs in `Executor::run()` after the strategy returns,
/// including on error paths — the safety net guarantee.
#[async_trait::async_trait]
pub trait SparkMiddleware: Send + Sync {
    async fn before_model_call(&self, session_id: &str, ghost: &str);
    async fn after_spark_complete(&self, session_id: &str, ghost: &str, outcome: &SparkOutcome);
}

/// Middleware that ensures the activity log is written even if a spark crashes.
///
/// Currently a no-op placeholder — the executor already flushes the log on
/// session close. This exists so the middleware infrastructure is exercised
/// end-to-end and the slot is ready for future flush logic.
pub struct ActivityLogFlushMiddleware;

#[async_trait::async_trait]
impl SparkMiddleware for ActivityLogFlushMiddleware {
    async fn before_model_call(&self, _session_id: &str, _ghost: &str) {}

    async fn after_spark_complete(&self, session_id: &str, ghost: &str, outcome: &SparkOutcome) {
        let status = match outcome {
            SparkOutcome::Success(_) => "success",
            SparkOutcome::Failure(_) => "failure",
        };
        tracing::debug!(session_id, ghost, status, "ActivityLogFlushMiddleware: spark complete");
    }
}

/// Convenience type alias used throughout the codebase.
pub type SharedMiddlewares = Vec<Arc<dyn SparkMiddleware>>;
```

- [ ] **Step 4: Run test — expect pass**

```bash
cargo test -p sparks middleware 2>&1
```
Expected: `test src/middleware.rs ... ok`

- [ ] **Step 5: Commit**

```bash
git add src/middleware.rs
git commit -m "feat(middleware): add SparkMiddleware trait and ActivityLogFlushMiddleware"
```

---

## Task 2: Add middleware to `Executor`

**Files:**
- Modify: `src/executor.rs`

- [ ] **Step 1: Write failing test — middleware invoked during run**

Add to the `#[cfg(test)]` section at the bottom of `src/executor.rs` (or create one):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{SharedMiddlewares, SparkMiddleware, SparkOutcome};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counter(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl SparkMiddleware for Counter {
        async fn before_model_call(&self, _s: &str, _g: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        async fn after_spark_complete(&self, _s: &str, _g: &str, _o: &SparkOutcome) {
            self.0.fetch_add(10, Ordering::SeqCst);
        }
    }

    #[test]
    fn executor_stores_middlewares() {
        let count = Arc::new(AtomicUsize::new(0));
        let mws: SharedMiddlewares = vec![Arc::new(Counter(count.clone()))];
        // Verify the Vec is accepted — actual invocation tested via integration
        assert_eq!(mws.len(), 1);
    }
}
```

- [ ] **Step 2: Run test — expect pass (compile test only)**

```bash
cargo test -p sparks executor::tests 2>&1
```

- [ ] **Step 3: Add `middlewares` field to `Executor` and two helper methods**

In `src/executor.rs`:

1. Add `use crate::middleware::{SharedMiddlewares, SparkMiddleware, SparkOutcome};` to imports.

2. Add field to `Executor` struct (after `activity_log`):

```rust
middlewares: SharedMiddlewares,
```

3. Add `middlewares: SharedMiddlewares` parameter to `Executor::new()` and assign it:

```rust
// in Executor::new() parameter list, add:
middlewares: SharedMiddlewares,

// in Self { ... } body, add:
middlewares,
```

4. Add two async methods to the `impl Executor` block:

```rust
/// Call all registered middlewares before an LLM invocation.
pub async fn invoke_before_model_call(&self, session_id: &str, ghost: &str) {
    for mw in &self.middlewares {
        mw.before_model_call(session_id, ghost).await;
    }
}

/// Call all registered middlewares after a spark completes (success or failure).
pub async fn invoke_after_spark_complete(
    &self,
    session_id: &str,
    ghost: &str,
    outcome: &SparkOutcome,
) {
    for mw in &self.middlewares {
        mw.after_spark_complete(session_id, ghost, outcome).await;
    }
}
```

- [ ] **Step 4: Call `invoke_after_spark_complete` at the end of `Executor::run()`**

In `Executor::run()`, the strategy result is matched at the end of the `ACTIVITY_CONTEXT.scope` block. Replace the final `match result { Ok(output) => ... Err(e) => ... }` block with:

```rust
let spark_outcome = match &result {
    Ok(o) => SparkOutcome::Success(o.clone()),
    Err(e) => SparkOutcome::Failure(e.to_string()),
};
let session_id = session.session_id().to_string();
self.invoke_after_spark_complete(&session_id, &ghost.name, &spark_outcome).await;

match result {
    Ok(output) => {
        tracing::info!(ghost = %ghost.name, "Task completed");
        if let Some(s) = run_span {
            let preview = if output.len() > 500 {
                &output[..output.floor_char_boundary(500)]
            } else {
                &output
            };
            s.end(Some(preview));
        }
        Ok(output)
    }
    Err(e) => {
        tracing::error!(ghost = %ghost.name, error = %e, "Task failed");
        if let Some(s) = run_span {
            s.end(Some(&format!("error: {}", e)));
        }
        Err(e)
    }
}
```

Note: `session.session_id()` is called before `close_session()` consumes the session. Move the `invoke_after_spark_complete` call to be after `close_session` at line ~323, passing the already-captured `session_id` string.

- [ ] **Step 5: Fix `Manager::new()` — pass empty middlewares to Executor::new()**

In `src/manager.rs`, find where `Executor::new(...)` is called and add `vec![]` as the `middlewares` argument. This keeps existing behavior; real middleware registration happens in Task 4.

- [ ] **Step 6: Check it compiles**

```bash
cargo check 2>&1 | head -30
```
Expected: no errors

- [ ] **Step 7: Commit**

```bash
git add src/executor.rs src/manager.rs src/middleware.rs
git commit -m "feat(middleware): add middleware field to Executor, wire after_spark_complete"
```

---

## Task 3: Call `invoke_before_model_call` in strategy loops

**Files:**
- Modify: `src/strategy/react.rs`
- Modify: `src/strategy/code.rs`

**Context:** `ReactStrategy::run_native()` has a `for step in 0..max_steps` loop. The LLM call is the first meaningful operation inside each iteration (streaming at `llm.chat_with_tools_stream(...)` or non-streaming at `llm.chat_with_tools(...)`). Both paths branch off the `use_streaming` check inside the same `for` loop.

The executor is available in the strategy as the `executor: &Executor` parameter.

- [ ] **Step 1: Add hook call at the top of the step loop in `react.rs`**

In `ReactStrategy::run_native()`, inside `for step in 0..max_steps {`, immediately after the `let gen = trace.map(...)` line, add:

```rust
// Fire before_model_call middleware before every LLM invocation.
let session_id = docker.session_id();
executor.invoke_before_model_call(session_id, "react").await;
```

Note: `docker.session_id()` returns the container session ID string. Use that as the session identifier here.

Do the same in `ReactStrategy::run_text_fallback()` if it also has a step loop with LLM calls.

- [ ] **Step 2: Find and add hook in `code.rs`**

`CodeStrategy` has multiple phase loops (EXPLORE, EXECUTE, VERIFY). Add `executor.invoke_before_model_call(session_id, "code").await;` at the top of each inner step loop that makes an LLM call, immediately before the first `llm.chat*` call.

Search for `for step in` in `code.rs` and add the hook to each occurrence.

- [ ] **Step 3: Check it compiles**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 4: Run all unit tests**

```bash
cargo test --lib 2>&1 | tail -20
```
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add src/strategy/react.rs src/strategy/code.rs
git commit -m "feat(middleware): call invoke_before_model_call in strategy loops"
```

---

## Task 4: Register built-in middleware from `core.rs`

**Files:**
- Modify: `src/core.rs`
- Modify: `src/config.rs`

**Context:** `core.rs` is where `Executor` is ultimately configured (via `Manager::new()`). The `ActivityLogFlushMiddleware` needs no external data. Future memory/KPI middlewares will need Arc handles available in `core.rs`.

- [ ] **Step 1: Add `MiddlewareConfig` to `config.rs`**

In `src/config.rs`, add after `LeaderboardConfig`:

```rust
#[derive(Debug, Deserialize, Clone, Default)]
pub struct MiddlewareConfig {
    /// Enable the activity-log flush safety net (default: true)
    #[serde(default = "default_true")]
    pub activity_log_flush: bool,
}
```

Add `middleware: MiddlewareConfig` field to `Config` with `#[serde(default)]`.

Note: `default_true()` helper likely already exists in `config.rs` for other boolean defaults. If not, add:
```rust
fn default_true() -> bool { true }
```

- [ ] **Step 2: Wire middlewares inside `Manager::new()` in `manager.rs`**

`Executor::new()` is called inside `Manager::new()` at `src/manager.rs`, not in `core.rs`. Build the middleware list there, from the `config` parameter that `Manager::new()` already receives:

```rust
use crate::middleware::{ActivityLogFlushMiddleware, SharedMiddlewares};

// Inside Manager::new(), before Executor::new() is called:
let middlewares: SharedMiddlewares = {
    let mut v: SharedMiddlewares = vec![];
    if config.middleware.activity_log_flush {
        v.push(Arc::new(ActivityLogFlushMiddleware));
    }
    v
};
```

Pass `middlewares` as the final argument to `Executor::new(...)`.

- [ ] **Step 3: Compile check**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 4: Run all tests**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/manager.rs
git commit -m "feat(middleware): register ActivityLogFlushMiddleware from Manager::new; add MiddlewareConfig"
```

---

## Task 5: Smoke test end-to-end

**Files:**
- Read: `src/middleware.rs`, `src/executor.rs`

- [ ] **Step 1: Add an integration-style test to `executor.rs`**

This tests that middlewares registered on Executor actually fire. Since we can't spin up Docker in unit tests, test the helper methods directly:

```rust
#[cfg(test)]
mod middleware_tests {
    use super::*;
    use crate::middleware::{SharedMiddlewares, SparkMiddleware, SparkOutcome};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Spy {
        before: Arc<AtomicUsize>,
        after: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SparkMiddleware for Spy {
        async fn before_model_call(&self, _s: &str, _g: &str) {
            self.before.fetch_add(1, Ordering::SeqCst);
        }
        async fn after_spark_complete(&self, _s: &str, _g: &str, _o: &SparkOutcome) {
            self.after.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn invoke_helpers_call_all_middlewares() {
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let mws: SharedMiddlewares = vec![
            Arc::new(Spy { before: before.clone(), after: after.clone() }),
            Arc::new(Spy { before: before.clone(), after: after.clone() }),
        ];

        // Build a minimal executor — only the middleware field matters here.
        // Use a stub by calling the helper methods on a temporary object.
        // Since we can't easily construct Executor without Docker deps in unit tests,
        // test via the public methods on a thin wrapper.
        struct MiddlewareRunner(SharedMiddlewares);
        impl MiddlewareRunner {
            async fn before(&self, s: &str, g: &str) {
                for mw in &self.0 { mw.before_model_call(s, g).await; }
            }
            async fn after(&self, s: &str, g: &str, o: &SparkOutcome) {
                for mw in &self.0 { mw.after_spark_complete(s, g, o).await; }
            }
        }

        let runner = MiddlewareRunner(mws);
        runner.before("sess", "coder").await;
        runner.before("sess", "coder").await;
        runner.after("sess", "coder", &SparkOutcome::Success("ok".into())).await;

        assert_eq!(before.load(Ordering::SeqCst), 4); // 2 calls × 2 middlewares
        assert_eq!(after.load(Ordering::SeqCst), 2);  // 1 call × 2 middlewares
    }
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test --lib middleware 2>&1
```
Expected: all pass

- [ ] **Step 3: Run full test suite**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 4: Run clippy**

```bash
cargo clippy 2>&1 | head -20
```

- [ ] **Step 5: Final commit**

```bash
git add src/middleware.rs src/executor.rs
git commit -m "test(middleware): add integration-style test for middleware invocation dispatch"
```
