use crate::error::{SparksError, Result};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

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
