# Mid-Run Message Injection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Messages sent to Slack, Teams, or Telegram while a spark is actively running get queued and injected as user-role messages before the next LLM call, instead of being dropped or starting a duplicate session.

**Architecture:** `Executor` holds a shared `Arc<Mutex<HashMap<String, VecDeque<InjectMessage>>>>` keyed by session ID. `CoreHandle` gets a clone of this Arc plus an `active_sessions` set. `CoreHandle::inject()` pushes to the queue; `CoreHandle::is_session_active()` lets frontends decide whether to inject or start fresh. Strategy loops drain the queue at each step start.

**Tech Stack:** Rust, tokio, Arc/Mutex

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/executor.rs` | Modify | Add inject queue + active session tracking; expose `inject_message()` |
| `src/core.rs` | Modify | Share inject queue with CoreHandle; add `CoreHandle::inject()` and `is_session_active()` |
| `src/strategy/react.rs` | Modify | Drain inject queue at each step start |
| `src/strategy/code.rs` | Modify | Same |
| `src/slack.rs` | Modify | Route mid-run messages to `inject()` |
| `src/teams.rs` | Modify | Same |
| `src/telegram.rs` | Modify | Same |
| `src/config.rs` | Modify | Add `max_queued_messages` to `ManagerConfig` |

---

## Task 1: Add inject queue and active session tracking to `Executor`

**Files:**
- Modify: `src/executor.rs`

- [ ] **Step 1: Write failing test**

Add to `src/executor.rs`:

```rust
#[cfg(test)]
mod inject_tests {
    use super::*;

    #[test]
    fn inject_queue_push_and_drain() {
        let queue: InjectQueue = Arc::new(Mutex::new(HashMap::new()));
        let session = "sess:user:chat";

        // Push a message
        {
            let mut q = queue.lock().unwrap();
            q.entry(session.to_string()).or_default().push_back("hello".to_string());
        }

        // Drain it
        let msgs = drain_inject_queue(&queue, session);
        assert_eq!(msgs, vec!["hello".to_string()]);

        // Queue should be empty now
        let msgs2 = drain_inject_queue(&queue, session);
        assert!(msgs2.is_empty());
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks inject_tests 2>&1 | head -20
```

- [ ] **Step 3: Add types and helpers to `executor.rs`**

At the top of `src/executor.rs`, add type aliases and a free function:

```rust
use std::collections::VecDeque;

pub type InjectQueue = Arc<Mutex<HashMap<String, VecDeque<String>>>>;

pub fn drain_inject_queue(queue: &InjectQueue, session_id: &str) -> Vec<String> {
    let mut q = match queue.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    q.get_mut(session_id)
        .map(|vq| vq.drain(..).collect())
        .unwrap_or_default()
}
```

Add to `Executor` struct (after `middlewares`):

```rust
pub inject_queue: InjectQueue,
pub active_sessions: Arc<Mutex<HashSet<String>>>,
```

Add `use std::collections::HashSet;` to imports.

Initialize in `Executor::new()`:

```rust
inject_queue: Arc::new(Mutex::new(HashMap::new())),
active_sessions: Arc::new(Mutex::new(HashSet::new())),
```

Add public method to `impl Executor`:

```rust
/// Push a message into the inject queue for a running session.
///// Returns false if session is not active (caller should start a new session instead).
/// This is the single authoritative path — CoreHandle::inject() delegates here, NOT directly to the queue.
pub fn inject_message(&self, session_id: &str, message: String) -> bool {
    let is_active = self.active_sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(session_id);
    if !is_active {
        return false;
    }
    let mut q = self.inject_queue
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let vq = q.entry(session_id.to_string()).or_default();
    vq.push_back(message);
    // Cap at max_queued_messages (hard-coded here, config in Task 5)
    while vq.len() > 10 {
        vq.pop_front();
    }
    true
}
```

In `Executor::run()`, register/deregister the session around the strategy call:

```rust
// Before strategy.run():
{
    let mut active = self.active_sessions.lock().unwrap_or_else(|p| p.into_inner());
    active.insert(session_id.clone());
}

// After strategy.run() returns (in close_session or after match):
{
    let mut active = self.active_sessions.lock().unwrap_or_else(|p| p.into_inner());
    active.remove(&session_id);
    // Clear any leftover queued messages
    self.inject_queue.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id);
}
```

The `session_id` is already captured as a `String` (from `session.session_id()`) in Task 1 of the middleware plan — reuse that pattern.

- [ ] **Step 4: Run test — expect pass**

```bash
cargo test -p sparks inject_tests 2>&1
```

- [ ] **Step 5: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 6: Commit**

```bash
git add src/executor.rs
git commit -m "feat(inject): add inject queue and active session tracking to Executor"
```

---

## Task 2: Expose `inject()` and `is_session_active()` on `CoreHandle`

**Files:**
- Modify: `src/core.rs`

**Context:** `CoreHandle` in `src/core.rs` is the object frontends hold. It communicates with the core via `mpsc::Sender<CoreRequest>`. We need to expose two new methods that operate directly on the shared queue — no channel round-trip needed.

- [ ] **Step 1: Write test for `is_session_active`**

Add to `src/core.rs`:

```rust
#[cfg(test)]
mod inject_tests {
    use super::*;

    #[test]
    fn session_active_returns_false_when_empty() {
        use crate::executor::InjectQueue;
        use std::collections::{HashMap, HashSet};
        let queue: InjectQueue = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let active: Arc<std::sync::Mutex<HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(HashSet::new()));

        // Simulate the check logic
        let is_active = active.lock().unwrap().contains("sess");
        assert!(!is_active);

        active.lock().unwrap().insert("sess".to_string());
        let is_active2 = active.lock().unwrap().contains("sess");
        assert!(is_active2);
    }
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p sparks core::inject_tests 2>&1
```

- [ ] **Step 3: Share inject capability between Executor and CoreHandle**

`CoreHandle::inject()` must delegate to `Executor::inject_message()` — not duplicate the lock logic — so cap enforcement is always applied. The cleanest way is to store the `InjectQueue` and `active_sessions` Arcs on `CoreHandle` (cloned from `Executor`) and expose a thin `ExecutorInjectHandle` wrapper:

Add a new small struct to `src/executor.rs`:

```rust
/// Thin handle to the executor's inject capability — safe to clone onto CoreHandle.
#[derive(Clone)]
pub struct ExecutorInjectHandle {
    queue: InjectQueue,
    active: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    max_queued: usize,
}

impl ExecutorInjectHandle {
    pub fn inject_message(&self, session_id: &str, message: String) -> bool {
        let is_active = self.active.lock().unwrap_or_else(|p| p.into_inner()).contains(session_id);
        if !is_active { return false; }
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let vq = q.entry(session_id.to_string()).or_default();
        vq.push_back(message);
        while vq.len() > self.max_queued { vq.pop_front(); }
        true
    }
    pub fn is_active(&self, session_id: &str) -> bool {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).contains(session_id)
    }
}
```

Add to `Executor`:

```rust
pub fn inject_handle(&self) -> ExecutorInjectHandle {
    ExecutorInjectHandle {
        queue: self.inject_queue.clone(),
        active: self.active_sessions.clone(),
        max_queued: self.max_queued_messages,
    }
}
```

Add to `Manager` (so `core.rs` can access it after building Manager):

```rust
pub fn inject_handle(&self) -> crate::executor::ExecutorInjectHandle {
    self.executor.inject_handle()
}
```

Add to `CoreHandle`:

```rust
executor_inject: crate::executor::ExecutorInjectHandle,
```

Pass it through at `CoreHandle` construction in `core.rs` via `manager.inject_handle()`.

- [ ] **Step 4: Add methods to `CoreHandle`**

```rust
impl CoreHandle {
    /// Returns true if a spark is actively running for the given session key.
    pub fn is_session_active(&self, session_key: &str) -> bool {
        self.active_sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(session_key)
    }

    /// Queue a message for injection into the next LLM step of a running session.
    /// Returns true if the session is active and the message was queued,
    /// false if no session is running (caller should start a new session instead).
    ///
    /// Delegates to `Executor::inject_message()` so cap enforcement and active-session
    /// checks are always applied from a single code path.
    pub fn inject(&self, session_key: &str, message: String) -> bool {
        self.executor_inject.inject_message(session_key, message)
    }
}
```

- [ ] **Step 5: Compile check**

```bash
cargo check 2>&1 | head -30
```

- [ ] **Step 6: Commit**

```bash
git add src/core.rs
git commit -m "feat(inject): add CoreHandle::inject() and is_session_active()"
```

---

## Task 3: Drain inject queue in strategy loops

**Files:**
- Modify: `src/strategy/react.rs`
- Modify: `src/strategy/code.rs`

- [ ] **Step 1: Write failing test for drain behavior**

Add to `src/strategy/react.rs` (or `src/executor.rs` — prefer to co-locate with `drain_inject_queue`):

```rust
#[cfg(test)]
mod inject_drain_tests {
    use crate::executor::{drain_inject_queue, InjectQueue};
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    #[test]
    fn drain_returns_messages_in_order() {
        let queue: InjectQueue = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut q = queue.lock().unwrap();
            let vq = q.entry("sess".to_string()).or_default();
            vq.push_back("first".to_string());
            vq.push_back("second".to_string());
        }
        let drained = drain_inject_queue(&queue, "sess");
        assert_eq!(drained, vec!["first", "second"]);
        // Queue empty after drain
        assert!(drain_inject_queue(&queue, "sess").is_empty());
    }
}
```

- [ ] **Step 2: Run test — expect pass** (drain_inject_queue already exists from Task 1)

```bash
cargo test --lib inject_drain 2>&1
```

- [ ] **Step 3: Add drain call in `react.rs`**

In `ReactStrategy::run_native()`, at the top of `for step in 0..max_steps {`, immediately before the LLM call (same location as the middleware hook from Plan 1, drain FIRST then middleware), add:

```rust
// Drain any messages injected while this step was executing.
// Must come before invoke_before_model_call so injected messages are in history first.
let session_id = docker.session_id();
let injected = crate::executor::drain_inject_queue(&executor.inject_queue, session_id);
for msg in injected {
    tracing::debug!(session_id, "Injecting mid-run message into history");
    history.push(ChatMessage::User(msg));
}
```

Do the same in `run_text_fallback()` if it has a step loop.

- [ ] **Step 4: Add drain call in `code.rs`**

`CodeStrategy` has multiple phase loops. Add the same drain block at the top of each inner step loop that builds history and calls the LLM.

- [ ] **Step 5: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 6: Run tests**

```bash
cargo test --lib inject 2>&1
```

- [ ] **Step 7: Commit**

```bash
git add src/strategy/react.rs src/strategy/code.rs
git commit -m "feat(inject): drain inject queue before each LLM call in strategy loops"
```

---

## Task 4: Wire into frontends

**Files:**
- Modify: `src/slack.rs`
- Modify: `src/teams.rs`
- Modify: `src/telegram.rs`

**Context:** Each frontend has a message handler that calls `handle.chat(session, input, confirmer)`. The session key is `session.session_key()` (format: `"platform:user_id:chat_id"`). The pattern is the same for all three:

```rust
// Before:
let events = handle.chat(session.clone(), input.clone(), confirmer).await;

// After:
let session_key = session.session_key();
if handle.is_session_active(&session_key) {
    // Spark is running — inject the message for pickup at next step
    handle.inject(&session_key, input.clone());
    // Optionally send acknowledgement to user
    send_message(&chat_id, "⏳ Message queued — spark will see it shortly").await;
} else {
    let events = handle.chat(session.clone(), input.clone(), confirmer).await;
    // ... existing event handling
}
```

- [ ] **Step 1: Find the message dispatch point in `slack.rs`**

Search for where `handle.chat(` is called in `src/slack.rs`. This is the point to wrap.

- [ ] **Step 2: Apply the inject routing pattern to `slack.rs`**

Wrap the `handle.chat()` call with the `is_session_active` check. Send a brief acknowledgement message to the Slack channel using the existing send helper when queuing.

- [ ] **Step 3: Apply the same to `teams.rs`**

Find `handle.chat(` in `src/teams.rs` and apply the same pattern.

- [ ] **Step 4: Apply the same to `telegram.rs`**

Find `handle.chat(` in `src/telegram.rs` and apply the same pattern.

- [ ] **Step 5: Compile check**

```bash
cargo check --features slack,teams,telegram 2>&1 | head -30
```

- [ ] **Step 6: Run all tests**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
git add src/slack.rs src/teams.rs src/telegram.rs
git commit -m "feat(inject): route mid-run messages to inject queue in all frontends"
```

---

## Task 5: Add `max_queued_messages` config

**Files:**
- Modify: `src/config.rs`
- Modify: `src/executor.rs`

- [ ] **Step 1: Add config field**

In `src/config.rs`, find `ManagerConfig` and add:

```rust
/// Max messages queued per session for mid-run injection. Oldest are dropped first.
#[serde(default = "default_max_queued_messages")]
pub max_queued_messages: usize,
```

Add:

```rust
fn default_max_queued_messages() -> usize { 5 }
```

- [ ] **Step 2: Pass through to Executor and use in `inject_message`**

Add `max_queued_messages: usize` to `Executor` struct, initialize from `ManagerConfig` in `Manager::new()`, and replace the hard-coded `10` in `inject_message()` with `self.max_queued_messages`.

- [ ] **Step 3: Add to `config.example.toml`**

```toml
[manager]
# Max messages queued per session for mid-run injection (default: 5).
# max_queued_messages = 5
```

- [ ] **Step 4: Write config test**

```rust
#[test]
fn manager_config_default_max_queued() {
    let cfg: ManagerConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.max_queued_messages, 5);
}
```

- [ ] **Step 5: Run test**

```bash
cargo test --lib config 2>&1 | grep queued
```

- [ ] **Step 6: Final compile + test**

```bash
cargo check 2>&1 | head -10
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/executor.rs config.example.toml
git commit -m "feat(inject): add max_queued_messages config; wire into Executor"
```
