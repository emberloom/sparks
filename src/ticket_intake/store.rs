use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{AthenaError, Result};

pub struct TicketIntakeStore {
    conn: Mutex<Connection>,
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
            .map_err(|e| AthenaError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
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
        title: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        conn.execute(
            "INSERT OR IGNORE INTO ticket_intake_log (dedup_key, provider, external_id, title)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![dedup_key, provider, external_id, title],
        )?;
        Ok(())
    }

    pub fn total_seen(&self) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock ticket intake store: {}", e)))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ticket_intake_log",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}
