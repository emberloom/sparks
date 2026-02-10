use rusqlite::Connection;
use std::sync::Mutex;

use crate::error::Result;

pub struct Memory {
    pub id: String,
    pub category: String,
    pub content: String,
    pub active: bool,
    pub created_at: String,
}

pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    /// Store a new memory
    pub fn store(&self, category: &str, content: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO memories (id, category, content) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, category, content],
        )?;
        Ok(id)
    }

    /// Search memories by keyword (simple LIKE match)
    pub fn search(&self, query: &str) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1 AND (content LIKE ?1 OR category LIKE ?1)
             ORDER BY created_at DESC LIMIT 10"
        )?;
        let rows = stmt.query_map(rusqlite::params![pattern], |row| {
            Ok(Memory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// List all active memories
    pub fn list(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1 ORDER BY created_at DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Memory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// Retire a memory (soft delete)
    pub fn retire(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE memories SET active = 0, updated_at = datetime('now') WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(updated > 0)
    }
}
