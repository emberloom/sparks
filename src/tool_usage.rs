use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{AthenaError, Result};

/// Aggregated usage statistics for a single tool.
#[derive(Debug, Clone)]
pub struct ToolUsageStats {
    pub tool_name: String,
    pub invocation_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub avg_duration_ms: f64,
    pub last_error: Option<String>,
    pub last_used: Option<String>,
}

impl ToolUsageStats {
    /// Success rate as a percentage (0.0–100.0). Returns 0 if no invocations.
    pub fn success_rate(&self) -> f64 {
        if self.invocation_count == 0 {
            return 0.0;
        }
        (self.success_count as f64 / self.invocation_count as f64) * 100.0
    }

    /// Format a compact summary for appending to tool descriptions.
    pub fn summary(&self) -> String {
        format!(
            "[used {}x, {:.0}% success, avg {:.0}ms]",
            self.invocation_count,
            self.success_rate(),
            self.avg_duration_ms,
        )
    }
}

/// Thread-safe, SQLite-backed store for tool invocation statistics.
/// One row per tool — aggregated counts, running average for duration.
pub struct ToolUsageStore {
    conn: Mutex<Connection>,
}

impl ToolUsageStore {
    /// Open a new store using the given SQLite connection.
    /// The connection should point to a database that has already run migration v9.
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Record a tool invocation outcome. Uses UPSERT to maintain one row per tool.
    /// Duration is tracked as a running average.
    pub fn record(
        &self,
        tool_name: &str,
        success: bool,
        duration_ms: f64,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| {
            AthenaError::Tool(format!("Failed to lock usage store: {}", e))
        })?;

        let success_inc: i64 = if success { 1 } else { 0 };
        let failure_inc: i64 = if success { 0 } else { 1 };

        conn.execute(
            "INSERT INTO tool_usage (tool_name, invocation_count, success_count, failure_count, avg_duration_ms, last_error, last_used, updated_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now'))
             ON CONFLICT(tool_name) DO UPDATE SET
               invocation_count = invocation_count + 1,
               success_count = success_count + ?2,
               failure_count = failure_count + ?3,
               avg_duration_ms = (avg_duration_ms * invocation_count + ?4) / (invocation_count + 1),
               last_error = COALESCE(?5, last_error),
               last_used = datetime('now'),
               updated_at = datetime('now')",
            rusqlite::params![tool_name, success_inc, failure_inc, duration_ms, error],
        )?;

        Ok(())
    }

    /// Get usage stats for a specific tool.
    pub fn get(&self, tool_name: &str) -> Result<Option<ToolUsageStats>> {
        let conn = self.conn.lock().map_err(|e| {
            AthenaError::Tool(format!("Failed to lock usage store: {}", e))
        })?;

        let mut stmt = conn.prepare(
            "SELECT tool_name, invocation_count, success_count, failure_count, avg_duration_ms, last_error, last_used
             FROM tool_usage WHERE tool_name = ?1",
        )?;

        let result = stmt.query_row(rusqlite::params![tool_name], |row| {
            Ok(ToolUsageStats {
                tool_name: row.get(0)?,
                invocation_count: row.get(1)?,
                success_count: row.get(2)?,
                failure_count: row.get(3)?,
                avg_duration_ms: row.get(4)?,
                last_error: row.get(5)?,
                last_used: row.get(6)?,
            })
        });

        match result {
            Ok(stats) => Ok(Some(stats)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get usage stats for all tools.
    pub fn all(&self) -> Result<Vec<ToolUsageStats>> {
        let conn = self.conn.lock().map_err(|e| {
            AthenaError::Tool(format!("Failed to lock usage store: {}", e))
        })?;

        let mut stmt = conn.prepare(
            "SELECT tool_name, invocation_count, success_count, failure_count, avg_duration_ms, last_error, last_used
             FROM tool_usage ORDER BY invocation_count DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ToolUsageStats {
                tool_name: row.get(0)?,
                invocation_count: row.get(1)?,
                success_count: row.get(2)?,
                failure_count: row.get(3)?,
                avg_duration_ms: row.get(4)?,
                last_error: row.get(5)?,
                last_used: row.get(6)?,
            })
        })?;

        let mut stats = Vec::new();
        for row in rows {
            stats.push(row?);
        }
        Ok(stats)
    }

    /// Get tools with failure rate above the given threshold (0.0–1.0).
    pub fn failing_tools(&self, threshold: f64) -> Result<Vec<ToolUsageStats>> {
        let all = self.all()?;
        Ok(all
            .into_iter()
            .filter(|s| {
                s.invocation_count > 0
                    && (s.failure_count as f64 / s.invocation_count as f64) > threshold
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> ToolUsageStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tool_usage (
                tool_name TEXT PRIMARY KEY,
                invocation_count INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                last_used TEXT,
                avg_duration_ms REAL NOT NULL DEFAULT 0.0,
                last_error TEXT,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .unwrap();
        ToolUsageStore::new(conn)
    }

    #[test]
    fn test_record_and_get() {
        let store = test_store();
        store.record("shell", true, 100.0, None).unwrap();
        store.record("shell", true, 200.0, None).unwrap();
        store.record("shell", false, 50.0, Some("timeout")).unwrap();

        let stats = store.get("shell").unwrap().unwrap();
        assert_eq!(stats.invocation_count, 3);
        assert_eq!(stats.success_count, 2);
        assert_eq!(stats.failure_count, 1);
        assert!((stats.success_rate() - 66.66).abs() < 1.0);
        assert_eq!(stats.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn test_get_nonexistent() {
        let store = test_store();
        assert!(store.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_all() {
        let store = test_store();
        store.record("shell", true, 100.0, None).unwrap();
        store.record("git", true, 50.0, None).unwrap();

        let all = store.all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_failing_tools() {
        let store = test_store();
        // Good tool: 90% success
        for _ in 0..9 {
            store.record("good_tool", true, 10.0, None).unwrap();
        }
        store.record("good_tool", false, 10.0, Some("err")).unwrap();

        // Bad tool: 80% failure
        store.record("bad_tool", true, 10.0, None).unwrap();
        for _ in 0..4 {
            store.record("bad_tool", false, 10.0, Some("err")).unwrap();
        }

        let failing = store.failing_tools(0.5).unwrap();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].tool_name, "bad_tool");
    }

    #[test]
    fn test_summary_format() {
        let stats = ToolUsageStats {
            tool_name: "shell".into(),
            invocation_count: 42,
            success_count: 40,
            failure_count: 2,
            avg_duration_ms: 230.0,
            last_error: None,
            last_used: None,
        };
        assert_eq!(stats.summary(), "[used 42x, 95% success, avg 230ms]");
    }

    #[test]
    fn test_running_average() {
        let store = test_store();
        store.record("tool", true, 100.0, None).unwrap();
        store.record("tool", true, 200.0, None).unwrap();

        let stats = store.get("tool").unwrap().unwrap();
        // (100 * 1 + 200) / 2 = 150
        assert!((stats.avg_duration_ms - 150.0).abs() < 1.0);
    }
}
