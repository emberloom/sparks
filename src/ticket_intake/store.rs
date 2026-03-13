use std::sync::Mutex;

use rusqlite::{params, Connection};

use crate::error::{SparksError, Result};

pub struct TicketIntakeStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct TicketSyncRecord {
    pub dedup_key: String,
    pub provider: String,
    pub external_id: String,
    pub issue_number: Option<String>,
    pub title: String,
    pub ci_monitor_status: Option<String>,
    pub task_status: String,
    pub task_goal: String,
    pub task_error: Option<String>,
    pub finished_at: Option<String>,
}

impl TicketIntakeStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    pub fn is_seen(&self, dedup_key: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SparksError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ticket_intake_log WHERE dedup_key = ?1",
            rusqlite::params![dedup_key],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_seen(
        &self,
        dedup_key: &str,
        provider: &str,
        external_id: &str,
        issue_number: Option<&str>,
        title: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SparksError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        conn.execute(
            "INSERT OR IGNORE INTO ticket_intake_log (dedup_key, provider, external_id, issue_number, title)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![dedup_key, provider, external_id, issue_number, title],
        )?;
        Ok(())
    }

    pub fn get_pending_syncs(&self, limit: usize) -> Result<Vec<TicketSyncRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SparksError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        let mut stmt = conn.prepare(
            "SELECT t.dedup_key,
                    t.provider,
                    t.external_id,
                    t.issue_number,
                    t.title,
                    t.ci_monitor_status,
                    o.status,
                    o.goal,
                    o.error,
                    o.finished_at
             FROM ticket_intake_log t
             JOIN autonomous_task_outcomes o
               ON o.task_id = 'ticket:' || t.dedup_key
            WHERE t.status = 'dispatched'
              AND o.status IN ('succeeded', 'failed')
              AND (t.ci_monitor_status IS NULL OR t.ci_monitor_status != 'monitoring')
            ORDER BY o.finished_at DESC
            LIMIT ?1",
        )?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(TicketSyncRecord {
                    dedup_key: row.get(0)?,
                    provider: row.get(1)?,
                    external_id: row.get(2)?,
                    issue_number: row.get(3)?,
                    title: row.get(4)?,
                    ci_monitor_status: row.get(5)?,
                    task_status: row.get(6)?,
                    task_goal: row.get(7)?,
                    task_error: row.get(8)?,
                    finished_at: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn update_status(&self, dedup_key: &str, status: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SparksError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        conn.execute(
            "UPDATE ticket_intake_log SET status = ?2 WHERE dedup_key = ?1",
            params![dedup_key, status],
        )?;
        Ok(())
    }
}
