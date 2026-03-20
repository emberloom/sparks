# Browser Sandbox Audit Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix five bugs from code review, complete the security event taxonomy, and wire browser security events into the Observer JSON stream for ML training.

**Architecture:** Browser security events are emitted via two channels simultaneously: `tracing` (structured stderr, `RUST_LOG`-filterable, zero-cost when disabled) and `ObserverHandle` (JSON-lines over UDS socket at `~/.sparks/observer.sock`, consumed by ML pipeline). The `McpInvoker` trait moves to `mcp.rs` and propagates through the full browse call stack so every layer is independently testable.

**Tech Stack:** Rust, `tracing` 0.1, `tracing-subscriber` 0.3, `async-trait` 0.1, `serde_json` 1, `tokio::sync::broadcast` (existing Observer infrastructure).

---

## Event Taxonomy

Before implementation, here is the complete set of events this system will emit after this plan is executed. This is the contract — every event listed here must be emitted, no more, no less.

### Performance contract
- `tracing` macros cost ~0 when filtered (gate checked before any formatting)
- **Exception:** `?` Debug format (e.g., `patterns = ?vec`) allocates a `String` even when filtered — all Vec/struct fields must use `%` (Display) or explicit `serde_json::to_string()` so the allocation only happens when actually logging
- `ObserverHandle::emit()` calls `broadcast::send()` once per event — if no receivers, the clone is dropped immediately (negligible); if an ML consumer is connected, one clone per event (also negligible for the event rate here)
- `observe_security` is gated behind `if let Some(obs)` — the `serde_json::json!(...)` allocation at each call site only occurs when an observer is present (production with ML consumer active). In tests, `observer` is `None` so no allocation occurs.
- Zero `unwrap()` calls in hot paths — all errors are logged and swallowed, never panic

### Event table

| Event name | Level | Channel | Trigger | Key fields |
|---|---|---|---|---|
| `browser.task_started` | INFO | tracing + Observer | `execute_browse` entry | `url`, `allowed_domains_count`, `max_navigations` |
| `browser.task_validation_failed` | WARN | tracing + Observer | URL/instruction rejected before session opens | `url`, `error` |
| `browser.task_completed` | INFO | tracing + Observer | Sub-agent returned JSON, schema passed | `url`, `final_url`, `navigations_used`, `injection_warnings_count` |
| `browser.task_failed` | WARN | tracing + Observer | Any error path in `execute_browse` | `url`, `error` |
| `browser.step_limit_reached` | WARN | tracing + Observer | `MAX_SUB_AGENT_STEPS` exhausted | `url`, `steps`, `navigations_used` |
| `browser.nav_allowed` | DEBUG | tracing only | URL passes network guard, nav counter incremented | `url`, `navigation_number` |
| `browser.nav_blocked` | WARN | tracing + Observer | Network guard rejects URL | `url`, `reason` |
| `browser.nav_limit_exceeded` | WARN | tracing + Observer | Navigation counter > `max_nav` | `current_url`, `limit`, `navigations_attempted` |
| `browser.tool_blocked` | WARN | tracing + Observer | Sub-agent calls non-allowlisted tool | `tool` |
| `browser.tool_failed` | WARN | tracing + Observer | MCP returns error on tool call | `tool`, `error` |
| `browser.injection_detected` | WARN | tracing + Observer | Visible injection pattern in `get_content` result | `domain`, `url`, `pattern`, `total_patterns_count` |

**Notes:**
- `browser.nav_allowed` is DEBUG-only (not Observer) — too high-frequency for ML signal, but useful during development with `RUST_LOG=sparks=debug`
- `browser.injection_detected` fires once **per matched pattern**, not once per page — this gives the ML pipeline one event per signal, with the specific regex that fired in the `pattern` field (not a Debug-formatted Vec)
- Observer `details` field carries a JSON string for ML consumption: `{"url":"...","domain":"...","pattern":"..."}`

---

## File Map

| File | Change |
|---|---|
| `src/mcp.rs` | Add `McpInvoker` trait + impl for `McpRegistry` (moved from browser_sandbox.rs) |
| `src/observer.rs` | Add `BrowserSecurity` variant to `ObserverCategory` |
| `src/browser_sandbox.rs` | Fix 5 bugs; complete event taxonomy; wire Observer; generalize `run_sub_agent_loop` + helpers |
| `Cargo.toml` | No changes needed — all required crates already present |

---

## Task 1: Move `McpInvoker` to `mcp.rs` and add `discovered_tools`

The trait abstracts MCP capabilities, not browser sandbox internals. Its natural home is `mcp.rs`. We also add `discovered_tools()` so `run_sub_agent_loop` can be made generic in Task 5.

**Files:**
- Modify: `src/mcp.rs`
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Write a failing compile-check (confirm current import path)**

```bash
grep -n "McpInvoker" src/browser_sandbox.rs
```
Expected: several lines — trait definition + impl + usage in tests.

- [ ] **Step 2: Add `McpInvoker` trait to `mcp.rs` (after existing imports, before `McpRegistry`)**

Add this block immediately before `pub struct McpRegistry`:

```rust
/// Abstraction over MCP tool invocation and discovery. Implemented by
/// [`McpRegistry`] in production and by test doubles in unit tests.
#[async_trait::async_trait]
pub trait McpInvoker: Send + Sync {
    async fn invoke_tool(
        &self,
        server: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> crate::error::Result<McpInvocationResult>;

    fn discovered_tools(&self) -> Vec<DiscoveredMcpTool>;
}

#[async_trait::async_trait]
impl McpInvoker for McpRegistry {
    async fn invoke_tool(
        &self,
        server: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> crate::error::Result<McpInvocationResult> {
        McpRegistry::invoke_tool(self, server, tool, args).await
    }

    fn discovered_tools(&self) -> Vec<DiscoveredMcpTool> {
        McpRegistry::discovered_tools(self)
    }
}
```

- [ ] **Step 3: Remove the duplicate `McpInvoker` trait block from `browser_sandbox.rs`**

Remove these lines (around line 500 in the current file — the trait definition and its `McpRegistry` impl):

```rust
/// Abstraction over MCP tool invocation, primarily for testing.
#[async_trait]
pub trait McpInvoker: Send + Sync {
    async fn invoke_tool(
        ...
    ) -> crate::error::Result<McpInvocationResult>;
}

#[async_trait]
impl McpInvoker for McpRegistry {
    ...
}
```

- [ ] **Step 4: Add the import to `browser_sandbox.rs`**

Change:
```rust
use crate::mcp::{McpInvocationResult, McpRegistry};
```
To:
```rust
use crate::mcp::{McpInvocationResult, McpInvoker, McpRegistry};
```

- [ ] **Step 5: Update `FakeMcp` in the test module to implement `discovered_tools`**

```rust
#[async_trait::async_trait]
impl McpInvoker for FakeMcp {
    async fn invoke_tool(
        &self,
        _server: &str,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> crate::error::Result<crate::mcp::McpInvocationResult> {
        Ok(crate::mcp::McpInvocationResult {
            success: true,
            output: self.response.clone(),
        })
    }

    fn discovered_tools(&self) -> Vec<crate::mcp::DiscoveredMcpTool> {
        vec![]
    }
}
```

- [ ] **Step 6: Verify it compiles and all browser tests pass**

```bash
cargo test browser 2>&1 | tail -10
```
Expected: `test result: ok. 36 passed`

- [ ] **Step 7: Commit**

```bash
git add src/mcp.rs src/browser_sandbox.rs
git commit -m "refactor(browser-audit): move McpInvoker trait to mcp.rs, add discovered_tools"
```

---

## Task 2: Fix navigation counter — increment only on successful URL validation

**Bug:** The nav counter is incremented before `check_url` is called. A blocked URL (e.g., localhost) still burns a navigation slot, allowing an adversarial sub-agent to exhaust the budget without ever navigating. The tracing warn also logs the wrong count.

**Files:**
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Write a failing test that proves the bug**

Add to the test module in `browser_sandbox.rs`:

```rust
#[tokio::test]
async fn blocked_navigate_does_not_consume_navigation_budget() {
    let sandbox = BrowserSandbox::new(vec![], "alpha".to_string(), true);
    let mcp = FakeMcp { response: "".to_string() };
    let mut navigations = 0u32;
    sandbox
        .execute_sub_agent_tool_call(
            &tc("navigate", json!({"url": "http://10.0.0.1/internal"})),
            &mcp, "pagerunner", "sess1",
            &mut navigations, 20, &mut vec![], &mut "".to_string(),
        )
        .await;
    assert_eq!(navigations, 0, "Blocked navigation must not consume budget");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test blocked_navigate_does_not_consume 2>&1 | tail -10
```
Expected: `FAILED` — `navigations` is 1 (bug confirmed).

- [ ] **Step 3: Fix the counter placement in `execute_sub_agent_tool_call`**

Change the navigate block from:

```rust
if tc.name == "navigate" {
    *navigations += 1;
    if *navigations > max_nav {
        ...return limit message...
    }
    if let Some(url) = tc.arguments.get("url").and_then(|u| u.as_str()) {
        if let Err(e) = self.network_guard.check_url(url) {
            return format!("Navigation blocked: {e}");
        }
        *final_url = url.to_string();
    }
}
```

To:

```rust
if tc.name == "navigate" {
    if let Some(url) = tc.arguments.get("url").and_then(|u| u.as_str()) {
        if let Err(e) = self.network_guard.check_url(url) {
            tracing::warn!(url = %url, reason = %e, "Browser navigation blocked by network guard");
            return format!("Navigation blocked: {e}");
        }
        *navigations += 1;
        if *navigations > max_nav {
            tracing::warn!(
                limit = max_nav,
                navigations = *navigations,
                current_url = %final_url,
                "Browser navigation limit exceeded"
            );
            return format!(
                "Navigation limit ({max_nav}) exceeded. Complete the task with data you have."
            );
        }
        *final_url = url.to_string();
        tracing::debug!(url = %url, navigation_number = *navigations, "Browser navigation allowed");
    }
}
```

- [ ] **Step 4: Verify the new test passes and no regressions**

```bash
cargo test browser 2>&1 | tail -10
```
Expected: `test result: ok. 37 passed`

- [ ] **Step 5: Commit**

```bash
git add src/browser_sandbox.rs
git commit -m "fix(browser-audit): increment nav counter only after URL passes network guard"
```

---

## Task 3: Fix async span — replace `span.enter()` with `.instrument()`

**Bug:** `Span::enter()` uses a thread-local guard. In async code, `tokio` can resume a future on a different thread after `.await`, leaving the guard on the wrong thread and corrupting span associations for all subsequent log events. The project already uses `.instrument()` in `core.rs:865`.

**Files:**
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Verify `tracing::Instrument` is available**

```bash
grep -n "use tracing" src/browser_sandbox.rs src/core.rs
```
Expected: `core.rs` has `use tracing::Instrument;` — we need to add the same to `browser_sandbox.rs`.

- [ ] **Step 2: Replace the span setup in `execute_browse`**

Remove:
```rust
let span = tracing::info_span!(
    "browser_sandbox",
    url = %task.url,
    max_navigations = task.max_navigations,
    allowed_domains = ?task.allowed_domains,
);
let _enter = span.enter();
```

Add `use tracing::Instrument;` to the imports at the top of the file, then rewrite `execute_browse` to use `.instrument()`:

```rust
pub async fn execute_browse(
    &self,
    task: &BrowseTask,
    mcp_registry: &McpRegistry,
    llm: &dyn LlmProvider,
    browser_server_name: &str,
) -> Result<BrowseResult, String> {
    let span = tracing::info_span!(
        "browser_sandbox",
        url = %task.url,
        max_navigations = task.max_navigations,
        allowed_domains_count = task.allowed_domains.len(),
    );

    async move {
        tracing::info!(url = %task.url, "Browse task started");

        if let Err(e) = self.validate_task(task) {
            tracing::warn!(url = %task.url, error = %e, "Browse task validation failed");
            return Err(e);
        }

        let session_id = self
            .open_browser_session(mcp_registry, browser_server_name)
            .await?;

        let result = self
            .run_sub_agent_loop(task, mcp_registry, llm, browser_server_name, &session_id)
            .await;

        let _ = self
            .close_browser_session(mcp_registry, browser_server_name, &session_id)
            .await;

        match &result {
            Ok(r) => tracing::info!(
                url = %task.url,
                final_url = %r.metadata.final_url,
                navigations = r.metadata.navigations_used,
                injection_warnings_count = r.metadata.injection_warnings.len(),
                "Browse task completed"
            ),
            Err(e) => tracing::warn!(url = %task.url, error = %e, "Browse task failed"),
        }

        result
    }
    .instrument(span)
    .await
}
```

Note: `allowed_domains = ?task.allowed_domains` replaced with `allowed_domains_count = task.allowed_domains.len()` — avoids a Debug-format allocation on every task start.

- [ ] **Step 3: Verify compile and tests pass**

```bash
cargo test browser 2>&1 | tail -10
```
Expected: `test result: ok. 37 passed`

- [ ] **Step 4: Commit**

```bash
git add src/browser_sandbox.rs
git commit -m "fix(browser-audit): use .instrument() for async span in execute_browse"
```

---

## Task 4: Fix injection event — emit one event per pattern with proper field types

**Bug:** `patterns = ?injection_warnings` uses Rust Debug format (`["(?i)ignore\\s+..."]`), which:
1. Allocates a `String` every call regardless of log level
2. Produces escaped, hard-to-parse output for ML consumers

**Fix:** Emit one `tracing::warn!` per matched pattern so each event has a single `pattern = %string` field (Display format, no allocation when filtered). Also wire injection events into the Observer channel.

**Files:**
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Locate the injection warn block in `execute_sub_agent_tool_call`**

```bash
grep -n "injection_warnings\|Prompt injection" src/browser_sandbox.rs
```

- [ ] **Step 2: Replace the current single warn with a per-pattern loop**

Change from:
```rust
if !injection_warnings.is_empty() {
    tracing::warn!(
        domain = %domain,
        url = %final_url,
        patterns = ?injection_warnings,
        count = injection_warnings.len(),
        "Prompt injection patterns detected in web content"
    );
}
warnings.extend(injection_warnings);
```

To:
```rust
let injection_count = injection_warnings.len();
for pattern in &injection_warnings {
    tracing::warn!(
        domain = %domain,
        url = %final_url,
        pattern = %pattern,
        total_patterns_count = injection_count,
        "Prompt injection pattern detected in web content"
    );
}
warnings.extend(injection_warnings);
```

- [ ] **Step 3: Verify the injection test still passes and the warning appears once per pattern**

```bash
cargo test get_content_is_sanitized 2>&1 | tail -10
```
Expected: `test result: ok. 1 passed`

- [ ] **Step 4: Commit**

```bash
git add src/browser_sandbox.rs
git commit -m "fix(browser-audit): emit one tracing event per injection pattern with Display format"
```

> **Agent note:** After this commit, `browser.injection_detected` emits to `tracing` only. The Observer emit for this event is added in Task 5, Step 4. Do **not** mark the event taxonomy as complete until Task 5 is done.

---

## Task 5: Add `BrowserSecurity` Observer category and wire security events

This task adds the ML export path. Browser security events (injection, nav blocks, tool blocks) are emitted to the `ObserverHandle` UDS JSON stream alongside `tracing`. Any process can `socat - UNIX-CONNECT:~/.sparks/observer.sock` and receive a JSON-lines stream suitable for ML training.

**Files:**
- Modify: `src/observer.rs` — add `BrowserSecurity` category
- Modify: `src/browser_sandbox.rs` — add `observer: Option<ObserverHandle>` to `BrowserSandbox`, wire emit calls

- [ ] **Step 1: Add `BrowserSecurity` to `ObserverCategory` in `observer.rs`**

In the enum, add after `CiMonitor`:
```rust
BrowserSecurity,
```

In `label()` match, add:
```rust
Self::BrowserSecurity => "BROWSER_SEC",
```

In `color()` match, add:
```rust
Self::BrowserSecurity => "\x1b[1;31m", // bright red — security events
```

- [ ] **Step 2: Add `observer` field to `BrowserSandbox`**

Change the struct definition (around line 190):
```rust
pub struct BrowserSandbox {
    network_guard: NetworkGuard,
    max_content_length: usize,
    profile: String,
    stealth: bool,
    observer: Option<crate::observer::ObserverHandle>,
}
```

Update `BrowserSandbox::new()`:
```rust
pub fn new(allowed_domains: Vec<String>, profile: String, stealth: bool) -> Self {
    Self {
        network_guard: if allowed_domains.is_empty() {
            NetworkGuard::default()
        } else {
            NetworkGuard::with_allowed_domains(allowed_domains)
        },
        max_content_length: browser_sanitizer::MAX_CONTENT_LENGTH,
        profile,
        stealth,
        observer: None,
    }
}

pub fn with_observer(mut self, observer: crate::observer::ObserverHandle) -> Self {
    self.observer = Some(observer);
    self
}
```

This keeps tests unchanged (they call `BrowserSandbox::new(...)` without an observer).

- [ ] **Step 3: Add a private emit helper to `BrowserSandbox`**

```rust
fn observe_security(&self, message: impl Into<String>, details: serde_json::Value) {
    if let Some(obs) = &self.observer {
        let details_str = serde_json::to_string(&details).unwrap_or_default();
        obs.emit(
            crate::observer::ObserverEvent::new(
                crate::observer::ObserverCategory::BrowserSecurity,
                message,
            )
            .with_details(details_str),
        );
    }
}
```

- [ ] **Step 4: Wire Observer calls at the security event sites**

At each of these sites, add an `observe_security` call alongside the existing `tracing::warn!`:

**nav blocked:**
```rust
self.observe_security(
    "Navigation blocked",
    serde_json::json!({"url": url, "reason": e.to_string()}),
);
```

**tool blocked:**
```rust
self.observe_security(
    "Disallowed tool blocked",
    serde_json::json!({"tool": tc.name}),
);
```

**nav limit exceeded:**
```rust
self.observe_security(
    "Navigation limit exceeded",
    serde_json::json!({"limit": max_nav, "current_url": final_url}),
);
```

**injection detected (inside the per-pattern loop from Task 4):**
```rust
self.observe_security(
    "Injection pattern detected",
    serde_json::json!({"domain": domain, "url": final_url, "pattern": pattern}),
);
```

**task validation failed:**
```rust
self.observe_security(
    "Task validation failed",
    serde_json::json!({"url": task.url, "error": e}),
);
```

- [ ] **Step 5: Wire observer through `for_ghost` into `BrowseTool` construction**

`for_ghost` in `tools.rs` (line 2062) does not currently have access to `ObserverHandle`. The observer lives on `Executor` and is passed down into `for_ghost`. We must add it as an optional parameter.

**5a. Add 9th parameter to `for_ghost` signature:**

```rust
pub fn for_ghost(
    ghost: &GhostConfig,
    dynamic_tools_path: Option<&Path>,
    mcp_registry: Option<Arc<McpRegistry>>,
    knobs: SharedKnobs,
    github_token: Option<String>,
    usage_store: Option<Arc<crate::tool_usage::ToolUsageStore>>,
    browser_sandbox_config: Option<&crate::config::BrowserSandboxConfig>,
    llm: Option<Arc<dyn crate::llm::LlmProvider>>,
    observer: Option<crate::observer::ObserverHandle>,   // NEW — 9th parameter
) -> Self {
```

**5b. Update the `BrowserSandbox::new()` call inside `for_ghost` (around line 2129):**

```rust
let sandbox = crate::browser_sandbox::BrowserSandbox::new(
    config.default_allowed_domains.clone(),
    config.profile.clone(),
    config.stealth,
);
let sandbox = if let Some(obs) = observer.clone() {
    sandbox.with_observer(obs)
} else {
    sandbox
};
```

**5c. Update all 11 test call sites in `tools.rs` that call `for_ghost` — each currently passes 8 arguments and needs `None` appended as the 9th:**

The following lines (search: `ToolRegistry::for_ghost`) all need `, None)` before the closing `)`:
- Lines 2576, 2595, 2611, 2620, 2647, 2675, 2696, 2719, 2753, 2776 (8-arg calls)
- Lines 2627 (may have named args — check and append `None`)

Change pattern: `, None, None)` → `, None, None, None)` for all 8-arg calls.

**5d. Update the production call site in `executor.rs`** where `ToolRegistry::for_ghost` is called. Find with:
```bash
grep -n "for_ghost" src/executor.rs
```
Pass `Some(self.observer.clone())` as the new 9th argument.

- [ ] **Step 5e: Verify compile:**

```bash
cargo build 2>&1 | grep "^error" | head -20
```
Expected: no errors. If you see "expected 9 arguments, found 8" that means a call site was missed — grep for all remaining 8-arg `for_ghost` calls.

- [ ] **Step 6: Verify all browser tests still pass (no observer in tests, observer is None)**

```bash
cargo test browser 2>&1 | tail -10
```
Expected: `test result: ok. 37 passed`

- [ ] **Step 7: Commit**

```bash
git add src/observer.rs src/browser_sandbox.rs src/tools.rs
git commit -m "feat(browser-audit): add BrowserSecurity Observer category and wire security events to UDS JSON stream"
```

---

## Task 6: Add missing step-limit event and strengthen tests

Adds the `browser.step_limit_reached` event and upgrades two existing tests with tighter assertions.

**Files:**
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Add step limit tracing + Observer event in `run_sub_agent_loop`**

Find the final `Err(...)` at the end of `run_sub_agent_loop` (line ~430):
```rust
Err(format!("Sub-agent exceeded maximum steps ({MAX_SUB_AGENT_STEPS})"))
```

Change to:
```rust
tracing::warn!(
    steps = MAX_SUB_AGENT_STEPS,
    url = %task.url,
    navigations_used = navigations,
    "Browse sub-agent step limit reached"
);
self.observe_security(
    "Sub-agent step limit reached",
    serde_json::json!({
        "url": task.url,
        "steps": MAX_SUB_AGENT_STEPS,
        "navigations_used": navigations,
    }),
);
Err(format!("Sub-agent exceeded maximum steps ({MAX_SUB_AGENT_STEPS})"))
```

Note: `run_sub_agent_loop` takes `&self` — `observe_security` is a `&self` method on `BrowserSandbox`, so this compiles without changes.

- [ ] **Step 2: Add call tracking to `FakeMcp`**

Replace the `FakeMcp` struct in the test module:

```rust
struct FakeMcp {
    response: String,
    call_count: std::sync::atomic::AtomicU32,
}

impl FakeMcp {
    fn new(response: &str) -> Self {
        Self {
            response: response.to_string(),
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }
    fn was_called(&self) -> bool {
        self.call_count.load(std::sync::atomic::Ordering::SeqCst) > 0
    }
}

#[async_trait::async_trait]
impl McpInvoker for FakeMcp {
    async fn invoke_tool(
        &self,
        _server: &str,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> crate::error::Result<crate::mcp::McpInvocationResult> {
        self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::mcp::McpInvocationResult {
            success: true,
            output: self.response.clone(),
        })
    }

    fn discovered_tools(&self) -> Vec<crate::mcp::DiscoveredMcpTool> {
        vec![]
    }
}
```

Update **all 5 tests** that construct `FakeMcp` with a struct literal to use `FakeMcp::new(...)` instead:
- `blocked_tool_returns_error_without_calling_mcp` → `FakeMcp::new("should not be called")`
- `navigate_to_localhost_is_blocked` → `FakeMcp::new("should not be called")`
- `navigation_limit_is_enforced` → `FakeMcp::new("should not be called")`
- `get_content_is_sanitized_and_injection_warned` → `FakeMcp::new(malicious_html)` (construct before the variable, or inline)
- `blocked_navigate_does_not_consume_navigation_budget` (added in Task 2) → `FakeMcp::new("")`

Search for remaining struct-literal constructions: `grep -n "FakeMcp {" src/browser_sandbox.rs` — must return 0 results after this step.

- [ ] **Step 3: Strengthen `blocked_tool_returns_error_without_calling_mcp`**

```rust
#[tokio::test]
async fn blocked_tool_returns_error_without_calling_mcp() {
    let sandbox = BrowserSandbox::new(vec![], "alpha".to_string(), true);
    let mcp = FakeMcp::new("should not be called");
    let result = sandbox
        .execute_sub_agent_tool_call(
            &tc("evaluate", json!({"code": "1+1"})),
            &mcp, "pagerunner", "sess1",
            &mut 0, 20, &mut vec![], &mut "".to_string(),
        )
        .await;
    assert!(result.contains("not available"), "got: {result}");
    assert!(!mcp.was_called(), "MCP must not be called for blocked tools");
}
```

- [ ] **Step 4: Strengthen `navigation_limit_is_enforced`**

```rust
#[tokio::test]
async fn navigation_limit_is_enforced() {
    let sandbox = BrowserSandbox::new(vec![], "alpha".to_string(), true);
    let mcp = FakeMcp::new("should not be called");
    let mut navigations = 5u32;
    let result = sandbox
        .execute_sub_agent_tool_call(
            &tc("navigate", json!({"url": "https://example.com/page6"})),
            &mcp, "pagerunner", "sess1",
            &mut navigations, 5, &mut vec![], &mut "".to_string(),
        )
        .await;
    assert!(result.contains("Navigation limit"), "got: {result}");
    assert_eq!(navigations, 6, "Counter must be incremented when limit is exceeded");
    assert!(!mcp.was_called(), "MCP must not be called when limit is exceeded");
}
```

- [ ] **Step 5: Run all browser tests**

```bash
cargo test browser 2>&1 | tail -10
```
Expected: `test result: ok. 38 passed` (one new test from Task 2 plus all existing)

- [ ] **Step 6: Commit**

```bash
git add src/browser_sandbox.rs
git commit -m "test(browser-audit): add call tracking to FakeMcp, strengthen blocked tool and nav limit tests"
```

---

## Task 7: Consolidate imports

**Files:**
- Modify: `src/browser_sandbox.rs`

- [ ] **Step 1: Move the mid-file `use` block to the top**

The block starting with `use std::sync::Arc;` (currently around line 494) was added inline when `BrowseTool` was introduced. Move all `use` declarations to the top of the file alongside the existing ones.

Final top-of-file imports should be:
```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use regex::Regex;
use tracing::Instrument;

use crate::browser_network_guard::NetworkGuard;
use crate::browser_sanitizer;
use crate::docker::DockerSession;
use crate::error::Result as SparksResult;
use crate::llm::{ChatMessage, ChatResponse, LlmProvider, ToolCall, ToolSchema};
use crate::mcp::{McpInvocationResult, McpInvoker, McpRegistry};
use crate::tools::{Tool, ToolResult};
```

- [ ] **Step 2: Verify compile and tests**

```bash
cargo test browser 2>&1 | tail -5
```
Expected: `test result: ok. 38 passed`

- [ ] **Step 3: Commit**

```bash
git add src/browser_sandbox.rs
git commit -m "refactor(browser-audit): consolidate imports to top of browser_sandbox.rs"
```

---

## Verification

After all tasks, run the full test suite:

```bash
cargo test 2>&1 | tail -5
```
Expected: all tests pass, no warnings about unused imports.

Confirm Observer events flow end-to-end (manual):
```bash
# Terminal 1: listen on UDS socket
socat - UNIX-CONNECT:$HOME/.sparks/observer.sock

# Terminal 2: trigger a browse task that hits a blocked URL or injection
# Expected in Terminal 1: JSON line with category "BrowserSecurity"
```

---

## What this does NOT include (future work)

- **Persistent audit log file:** `tracing-appender` for a rolling JSONL file. Implement when there's a consumer that needs replay (not yet needed — UDS stream is sufficient for live ML training).
- **Making `run_sub_agent_loop` generic over `McpInvoker`:** Requires `FakeMcp::discovered_tools()` to return real `ToolSchema`s and a fake `LlmProvider`. Worth doing for full loop-level testing, but deferred — the existing tests cover the security enforcement layer completely.
- **`tracing-json` subscriber:** For structured JSON to stderr (vs. the human-readable compact format). Add when feeding logs into a log aggregator.
