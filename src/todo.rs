use crate::error::{SparksError, Result};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use serde_json::Value;

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
            .map(|item| { item.done = true; })
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Render as a progress block for frontend display.
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

pub type TodoSessions = Arc<Mutex<HashMap<String, TodoList>>>;

/// Helper used by TodoWriteTool and tests.
pub fn execute_todo_write_for_session(
    sessions: &TodoSessions,
    session_key: &str,
    params: &Value,
) -> Result<String> {
    let items: Vec<String> = params["items"]
        .as_array()
        .ok_or_else(|| SparksError::Tool("todo_write requires 'items' array".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if items.is_empty() {
        return Err(SparksError::Tool("todo_write: items is empty".into()));
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
    params: &Value,
) -> Result<String> {
    let index = params["index"]
        .as_u64()
        .ok_or_else(|| SparksError::Tool("todo_check requires 'index' integer".into()))?
        as usize;
    sessions.lock().unwrap_or_else(|p| p.into_inner())
        .entry(session_key.to_string())
        .or_default()
        .check(index)?;
    Ok(format!("Item {} marked done.", index))
}

pub struct TodoWriteTool {
    pub sessions: TodoSessions,
}

pub struct TodoCheckTool {
    pub sessions: TodoSessions,
}

#[async_trait::async_trait]
impl crate::tools::Tool for TodoWriteTool {
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
    async fn execute(&self, _session: &crate::docker::DockerSession, params: &Value) -> Result<crate::tools::ToolResult> {
        let session_key = crate::executor::Executor::current_activity_context()
            .map(|c| c.session_key)
            .unwrap_or_else(|| "unknown".to_string());
        let output = execute_todo_write_for_session(&self.sessions, &session_key, params)?;
        Ok(crate::tools::ToolResult { success: true, output })
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for TodoCheckTool {
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
    async fn execute(&self, _session: &crate::docker::DockerSession, params: &Value) -> Result<crate::tools::ToolResult> {
        let session_key = crate::executor::Executor::current_activity_context()
            .map(|c| c.session_key)
            .unwrap_or_else(|| "unknown".to_string());
        let output = execute_todo_check_for_session(&self.sessions, &session_key, params)?;
        Ok(crate::tools::ToolResult { success: true, output })
    }
}

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
