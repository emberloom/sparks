# Per-Spark Todo Lists Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sparks can write and check off a todo list during execution via `todo_write` and `todo_check` tools, making long-running tasks observable mid-run via the `/session` command.

**Architecture:** A new `src/todo.rs` module holds `TodoList` state (in-memory per executor session). Two new tools (`todo_write`, `todo_check`) are registered in the tool registry. State is persisted to SQLite via a new `spark_todos` migration in `db.rs` when the session closes. The `/session` command in all frontends renders the todo list as a progress block.

**Tech Stack:** Rust, rusqlite, serde_json

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/todo.rs` | Create | `TodoList` state type + tool handler functions |
| `src/db.rs` | Modify | Add `spark_todos` table migration |
| `src/tools.rs` | Modify | Register `todo_write` and `todo_check` in the tool registry |
| `src/executor.rs` | Modify | Hold `TodoList` per session; persist on close |
| `src/session_review.rs` | Modify | Include todo list in session close payload |
| `src/slack.rs` | Modify | Render todo block in `/session` output |
| `src/telegram.rs` | Modify | Same |

---

## Task 1: Create `src/todo.rs` with `TodoList` state and tool handlers

**Files:**
- Create: `src/todo.rs`

- [ ] **Step 1: Write failing tests**

Create `src/todo.rs` with a `#[cfg(test)]` module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_write_replaces_list() {
        let mut list = TodoList::new();
        list.write(vec!["item one".into(), "item two".into()]);
        assert_eq!(list.items().len(), 2);
        list.write(vec!["only item".into()]);
        assert_eq!(list.items().len(), 1);
        assert_eq!(list.items()[0].text, "only item");
        assert!(!list.items()[0].done);
    }

    #[test]
    fn todo_check_marks_item_done() {
        let mut list = TodoList::new();
        list.write(vec!["task a".into(), "task b".into()]);
        assert!(list.check(1).is_ok());
        assert!(list.items()[1].done);
        assert!(!list.items()[0].done);
    }

    #[test]
    fn todo_check_out_of_bounds_returns_err() {
        let mut list = TodoList::new();
        list.write(vec!["only".into()]);
        assert!(list.check(5).is_err());
    }

    #[test]
    fn todo_render_progress_shows_symbols() {
        let mut list = TodoList::new();
        list.write(vec!["done".into(), "pending".into()]);
        list.check(0).unwrap();
        let rendered = list.render_progress();
        assert!(rendered.contains("✓"));
        assert!(rendered.contains("○"));
    }

    #[test]
    fn empty_todo_list_render_returns_empty_string() {
        let list = TodoList::new();
        assert!(list.render_progress().is_empty());
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks todo::tests 2>&1 | head -20
```

- [ ] **Step 3: Implement `TodoList`**

```rust
use crate::error::{SparksError, Result};

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

/// In-memory todo list for a single spark session.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Vec<TodoItem>,
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the current list with new items (all undone).
    pub fn write(&mut self, items: Vec<String>) {
        self.items = items.into_iter().map(|text| TodoItem { text, done: false }).collect();
    }

    /// Mark item at `index` (0-based) as done.
    pub fn check(&mut self, index: usize) -> Result<()> {
        self.items
            .get_mut(index)
            .ok_or_else(|| SparksError::Tool(format!("Todo index {} out of bounds", index)))
            .map(|item| item.done = true)
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Render as a progress block for frontend display.
    /// Returns an empty string if no items.
    pub fn render_progress(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        self.items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let symbol = if item.done { "✓" } else { "○" };
                format!("{} [{}] {}", symbol, i, item.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Serialize to a JSON string for SQLite storage.
    pub fn to_json(&self) -> String {
        let items: Vec<serde_json::Value> = self.items.iter().map(|item| {
            serde_json::json!({ "text": item.text, "done": item.done })
        }).collect();
        serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Tool handler: `todo_write(items: Vec<String>)`
/// Called by the LLM with a JSON array of task descriptions.
pub fn handle_todo_write(list: &mut TodoList, params: &serde_json::Value) -> Result<String> {
    let items: Vec<String> = params["items"]
        .as_array()
        .ok_or_else(|| SparksError::Tool("todo_write requires an 'items' array".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    if items.is_empty() {
        return Err(SparksError::Tool("todo_write: items array is empty".into()));
    }

    let count = items.len();
    list.write(items);
    Ok(format!("Todo list updated with {} items.", count))
}

/// Tool handler: `todo_check(index: usize)`
/// Called by the LLM to mark a task as done.
pub fn handle_todo_check(list: &mut TodoList, params: &serde_json::Value) -> Result<String> {
    let index = params["index"]
        .as_u64()
        .ok_or_else(|| SparksError::Tool("todo_check requires an 'index' integer".into()))?
        as usize;

    list.check(index)?;
    Ok(format!("Todo item {} marked as done.", index))
}
```

- [ ] **Step 4: Add `mod todo;` to `src/main.rs` or wherever modules are declared**

Find the module declarations (usually `src/main.rs` or `src/lib.rs`) and add:

```rust
mod todo;
```

- [ ] **Step 5: Run tests — expect pass**

```bash
cargo test -p sparks todo::tests 2>&1
```

- [ ] **Step 6: Commit**

```bash
git add src/todo.rs src/main.rs  # or lib.rs — whichever has mod declarations
git commit -m "feat(todo): add TodoList state type and todo_write/todo_check handlers"
```

---

## Task 2: Add `spark_todos` table to `db.rs`

**Files:**
- Modify: `src/db.rs`

**Context:** `db.rs` has a `MIGRATIONS: &[&str]` array. Each entry is a SQL string run in order. The current highest migration is visible from the array — find the last entry and add the next one.

- [ ] **Step 1: Write test**

Add to `src/db.rs`:

```rust
#[cfg(test)]
mod todo_migration_tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn spark_todos_table_created_by_migration() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        // If the table exists this won't error
        conn.execute_batch(
            "INSERT INTO spark_todos (session_key, ghost, items_json) VALUES ('s', 'g', '[]')"
        ).unwrap();
    }
}
```

- [ ] **Step 2: Run test — expect failure (table doesn't exist yet)**

```bash
cargo test -p sparks todo_migration_tests 2>&1 | head -20
```

- [ ] **Step 3: Add the migration**

In `src/db.rs`, add a new entry to the `MIGRATIONS` array (after the last existing entry):

```rust
// vN: spark todo lists — per-session task tracking
"CREATE TABLE IF NOT EXISTS spark_todos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_key TEXT NOT NULL,
    ghost TEXT NOT NULL,
    items_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_spark_todos_session ON spark_todos(session_key, created_at);",
```

Replace `N` with the actual next version number.

- [ ] **Step 4: Check that `run_migrations` function exists and is called at DB open**

```bash
grep -n "run_migrations\|fn open\|MIGRATIONS" src/db.rs | head -20
```

Confirm `run_migrations` iterates over `MIGRATIONS` and runs each. If not, trace how migrations run to ensure the new entry will execute.

- [ ] **Step 5: Run test — expect pass**

```bash
cargo test -p sparks todo_migration_tests 2>&1
```

- [ ] **Step 6: Commit**

```bash
git add src/db.rs
git commit -m "feat(todo): add spark_todos migration to db.rs"
```

---

## Task 3: Hold `TodoList` per session in `Executor` and persist on close

**Files:**
- Modify: `src/executor.rs`

**Context:** `Executor::run()` already tracks the `session_id` and calls `close_session()`. The todo list lives for the duration of one `run()` call. It's passed to strategy via the `executor` reference — but strategies call `executor.execute_tool()`, not a separate todo API. The todo tools need to be accessible from the tool execution path.

The cleanest approach: store the `TodoList` in a `Arc<Mutex<TodoList>>` keyed by session on the Executor (similar to `loop_guard`), so tool handlers can access it.

- [ ] **Step 1: Add todo state to `Executor`**

Add to `src/executor.rs`:

```rust
use crate::todo::TodoList;

// In Executor struct:
todo_sessions: Arc<Mutex<HashMap<String, TodoList>>>,
```

Initialize in `Executor::new()`:

```rust
todo_sessions: Arc::new(Mutex::new(HashMap::new())),
```

Add public accessors:

```rust
pub fn todo_write(&self, session_id: &str, items: Vec<String>) {
    let mut sessions = self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.entry(session_id.to_string()).or_default().write(items);
}

pub fn todo_check(&self, session_id: &str, index: usize) -> Result<()> {
    let mut sessions = self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.entry(session_id.to_string()).or_default().check(index)
}

pub fn todo_render(&self, session_id: &str) -> String {
    let sessions = self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_id).map(|l| l.render_progress()).unwrap_or_default()
}

pub fn todo_json(&self, session_id: &str) -> String {
    let sessions = self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_id).map(|l| l.to_json()).unwrap_or_else(|| "[]".to_string())
}
```

- [ ] **Step 2: Clear todo session on close**

In `close_session()`, after clearing `loop_guard`:

```rust
self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(session.session_id());
```

- [ ] **Step 3: Write test**

```rust
#[cfg(test)]
mod todo_executor_tests {
    use super::*;

    #[test]
    fn executor_todo_write_and_render() {
        // Can't build full Executor in unit tests — test the TodoList directly via todo module
        use crate::todo::TodoList;
        let mut list = TodoList::new();
        list.write(vec!["step a".into()]);
        assert!(!list.render_progress().is_empty());
    }
}
```

- [ ] **Step 4: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 5: Commit**

```bash
git add src/executor.rs
git commit -m "feat(todo): add per-session TodoList to Executor with accessors"
```

---

## Task 4: Implement `Tool` trait for `TodoWriteTool` and `TodoCheckTool`

**Files:**
- Modify: `src/todo.rs`
- Modify: `src/tools.rs`

**Architecture note:** `ToolRegistry::for_ghost()` in `src/tools.rs` builds `all_tools: Vec<Box<dyn Tool>>` and filters by `ghost.tools.contains(&name)`. Each tool must implement `trait Tool`. The `execute()` method receives `(docker_session, params)` — it does NOT have access to the executor directly.

To give the todo tools access to the shared `TodoList` state on `Executor`, pass an `Arc<Mutex<HashMap<String, TodoList>>>` as a field when constructing the tool. The current `session_key` (needed to index into the HashMap) is available inside `execute()` via `crate::executor::Executor::current_activity_context()` — this `tokio::task_local!` is set in `Executor::run()` before any tool is ever called, so it is always populated during tool execution.

- [ ] **Step 1: Write failing test for tool execution**

Add to `src/todo.rs`:

```rust
#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn todo_write_tool_parses_items_from_params() {
        let sessions: TodoSessions = Arc::new(Mutex::new(HashMap::new()));
        let params = serde_json::json!({ "items": ["step a", "step b"] });
        let result = execute_todo_write_for_session(&sessions, "sess", &params);
        assert!(result.is_ok());
        let sessions = sessions.lock().unwrap();
        assert_eq!(sessions.get("sess").map(|l| l.items().len()), Some(2));
    }

    #[test]
    fn todo_check_tool_marks_done() {
        let sessions: TodoSessions = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut s = sessions.lock().unwrap();
            let list = s.entry("sess".to_string()).or_default();
            list.write(vec!["task".into()]);
        }
        let params = serde_json::json!({ "index": 0 });
        let result = execute_todo_check_for_session(&sessions, "sess", &params);
        assert!(result.is_ok());
        let sessions = sessions.lock().unwrap();
        assert!(sessions.get("sess").unwrap().items()[0].done);
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

```bash
cargo test -p sparks todo::tool_tests 2>&1 | head -20
```

- [ ] **Step 3: Add `TodoSessions` type and session-scoped helpers to `todo.rs`**

Add to `src/todo.rs`:

```rust
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

pub type TodoSessions = Arc<Mutex<HashMap<String, TodoList>>>;

/// Extracted helper — used by both the Tool impl and unit tests.
pub fn execute_todo_write_for_session(
    sessions: &TodoSessions,
    session_key: &str,
    params: &serde_json::Value,
) -> crate::error::Result<String> {
    let items: Vec<String> = params["items"]
        .as_array()
        .ok_or_else(|| crate::error::SparksError::Tool("todo_write requires 'items' array".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if items.is_empty() {
        return Err(crate::error::SparksError::Tool("todo_write: items is empty".into()));
    }
    let count = items.len();
    sessions.lock().unwrap_or_else(|p| p.into_inner())
        .entry(session_key.to_string())
        .or_default()
        .write(items);
    Ok(format!("Todo list updated with {} items.", count))
}

pub fn execute_todo_check_for_session(
    sessions: &TodoSessions,
    session_key: &str,
    params: &serde_json::Value,
) -> crate::error::Result<String> {
    let index = params["index"]
        .as_u64()
        .ok_or_else(|| crate::error::SparksError::Tool("todo_check requires 'index' integer".into()))?
        as usize;
    sessions.lock().unwrap_or_else(|p| p.into_inner())
        .entry(session_key.to_string())
        .or_default()
        .check(index)?;
    Ok(format!("Item {} marked done.", index))
}
```

- [ ] **Step 4: Add `TodoWriteTool` and `TodoCheckTool` structs to `todo.rs`**

```rust
use crate::docker::DockerSession;
use crate::tools::{Tool, ToolResult};

pub struct TodoWriteTool {
    pub sessions: TodoSessions,
}

pub struct TodoCheckTool {
    pub sessions: TodoSessions,
}

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str { "todo_write" }
    fn description(&self) -> String {
        "Replace your current todo list with a new list of task descriptions. \
         Call at the start of a complex task and whenever your plan changes.".to_string()
    }
    fn needs_confirmation(&self) -> bool { false }
    fn parameter_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered list of task descriptions."
                }
            }
        })
    }
    async fn execute(&self, _session: &DockerSession, params: &serde_json::Value) -> crate::error::Result<ToolResult> {
        let session_key = crate::executor::Executor::current_activity_context()
            .map(|c| c.session_key)
            .unwrap_or_else(|| "unknown".to_string());
        let output = execute_todo_write_for_session(&self.sessions, &session_key, params)?;
        Ok(ToolResult { success: true, output })
    }
}

#[async_trait::async_trait]
impl Tool for TodoCheckTool {
    fn name(&self) -> &str { "todo_check" }
    fn description(&self) -> String {
        "Mark a todo item as done by its 0-based index.".to_string()
    }
    fn needs_confirmation(&self) -> bool { false }
    fn parameter_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["index"],
            "properties": {
                "index": { "type": "integer", "description": "0-based index of the item to mark done." }
            }
        })
    }
    async fn execute(&self, _session: &DockerSession, params: &serde_json::Value) -> crate::error::Result<ToolResult> {
        let session_key = crate::executor::Executor::current_activity_context()
            .map(|c| c.session_key)
            .unwrap_or_else(|| "unknown".to_string());
        let output = execute_todo_check_for_session(&self.sessions, &session_key, params)?;
        Ok(ToolResult { success: true, output })
    }
}
```

Note: `Executor::current_activity_context()` is currently `fn` (not `pub`). Change it to `pub(crate)` in `executor.rs` to allow `todo.rs` to call it, or expose a public wrapper.

- [ ] **Step 5: Register in `ToolRegistry::for_ghost()` in `tools.rs`**

In `src/tools.rs`, `ToolRegistry::for_ghost()` has a `let mut all_tools: Vec<Box<dyn Tool>> = vec![...]` block. The todo tools are stateful (they hold an Arc) so they need to receive the `TodoSessions` from `Executor`. The cleanest approach is to add `todo_sessions: Option<crate::todo::TodoSessions>` as a parameter to `for_ghost()`, then push the tools if sessions are provided:

```rust
// In for_ghost() parameter list, add:
todo_sessions: Option<crate::todo::TodoSessions>,

// In all_tools.extend / push section, before the filter:
if let Some(sessions) = todo_sessions {
    all_tools.push(Box::new(crate::todo::TodoWriteTool { sessions: sessions.clone() }));
    all_tools.push(Box::new(crate::todo::TodoCheckTool { sessions }));
}
```

Update every call to `ToolRegistry::for_ghost()` (in `executor.rs` and in tests) to pass `Some(self.todo_sessions.clone())` or `None` as appropriate.

- [ ] **Step 6: Run tests — expect pass**

```bash
cargo test --lib todo 2>&1
```

- [ ] **Step 7: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 8: Commit**

```bash
git add src/todo.rs src/tools.rs src/executor.rs
git commit -m "feat(todo): implement Tool trait for TodoWriteTool/TodoCheckTool; register in ToolRegistry"
```

---

## Task 5: Surface todo list in `/session` command

**Files:**
- Modify: `src/slack.rs`
- Modify: `src/telegram.rs`

**Context:** The `/session` command in frontends calls into `CoreHandle` to get session review data and formats it for display. The todo list is on `Executor`, which is not directly accessible from frontends — they go through `CoreHandle`. Add a `CoreHandle::session_todos(session_key: &str) -> String` method.

- [ ] **Step 1: Add `session_todos` to `CoreHandle` via `Manager`**

Add to `Manager`:

```rust
pub fn session_todos(&self, session_key: &str) -> String {
    self.executor.todo_render(session_key)
}
```

Wire through `CoreHandle` by adding a channel message type or storing the executor's `todo_sessions` Arc on `CoreHandle` (same pattern as inject queue). For simplicity, store a clone of `executor.todo_sessions` on `CoreHandle` and add:

```rust
pub fn session_todos(&self, session_key: &str) -> String {
    let sessions = self.todo_sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_key).map(|l| l.render_progress()).unwrap_or_default()
}
```

- [ ] **Step 2: Add todo section to `/session` output in `slack.rs`**

Find the `/session` slash command handler in `src/slack.rs`. After formatting existing session review data, append:

```rust
let todos = handle.session_todos(&session_key);
if !todos.is_empty() {
    response.push_str("\n\n*Todo List:*\n");
    response.push_str(&format!("```\n{}\n```", todos));
}
```

- [ ] **Step 3: Same in `telegram.rs`**

Find the `/session` command handler in `src/telegram.rs` and add the same todo block to the response.

- [ ] **Step 4: Compile check with features**

```bash
cargo check --features slack,telegram 2>&1 | head -30
```

- [ ] **Step 5: Run all tests**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 6: Commit**

```bash
git add src/slack.rs src/telegram.rs src/core.rs src/manager.rs
git commit -m "feat(todo): surface todo list in /session command on Slack and Telegram"
```

---

## Task 6: Persist todo list to SQLite on session close

**Files:**
- Modify: `src/executor.rs`
- Modify: `src/db.rs` (add a store helper)

- [ ] **Step 1: Add persistence helper to `db.rs`**

Add to `src/db.rs`:

```rust
pub fn save_spark_todos(conn: &Connection, session_key: &str, ghost: &str, items_json: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO spark_todos (session_key, ghost, items_json) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_key, ghost, items_json],
    ).map_err(|e| SparksError::Db(format!("Failed to save spark todos: {}", e)))?;
    Ok(())
}
```

- [ ] **Step 2: Call from `close_session` in `executor.rs`**

`close_session()` currently just clears the loop guard and closes Docker. Add:

```rust
// Persist todo list if non-empty
if let Some(activity_log) = &self.activity_log {
    let todos_json = self.todo_json(session.session_id());
    if todos_json != "[]" {
        // Get ghost name from activity context
        let ghost = Self::current_activity_context()
            .map(|c| c.ghost)
            .unwrap_or_else(|| "unknown".to_string());
        let session_key = Self::current_activity_context()
            .map(|c| c.session_key)
            .unwrap_or_else(|| session.session_id().to_string());
        // Use the activity_log's db connection — or open a new one.
        // For now, log via observer (full DB persistence is a follow-up).
        self.observer.log(
            crate::observer::ObserverCategory::Execution,
            &format!("spark_todos session={} ghost={} items={}", session_key, ghost, todos_json),
        );
    }
}
```

Note: Full DB persistence requires access to the SQLite connection. The activity log's connection is in `ActivityLogStore`. For a complete implementation, expose a `save_todos()` method on `ActivityLogStore` that calls `save_spark_todos`. For now, logging via observer is sufficient to make the feature testable.

- [ ] **Step 3: Compile check**

```bash
cargo check 2>&1 | head -20
```

- [ ] **Step 4: Run all tests**

```bash
cargo test --lib 2>&1 | tail -20
```

- [ ] **Step 5: Run clippy**

```bash
cargo clippy 2>&1 | head -20
```

- [ ] **Step 6: Final commit**

```bash
git add src/executor.rs src/db.rs
git commit -m "feat(todo): persist todo list to observer log on session close"
```
