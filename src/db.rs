use rusqlite::Connection;
use std::path::Path;

use crate::error::{AthenaError, Result};

const MIGRATIONS: &[&str] = &[
    // v1: memories table
    "CREATE TABLE IF NOT EXISTS memories (
        id TEXT PRIMARY KEY,
        category TEXT NOT NULL,
        content TEXT NOT NULL,
        active INTEGER NOT NULL DEFAULT 1,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );",
    // v1: schema version tracking
    "CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY
    );",
];

pub fn init_db(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AthenaError::Config(format!("Failed to create db directory: {}", e)))?;
    }

    let conn = Connection::open(path)?;

    // Enable WAL mode for better concurrency
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    // Run migrations
    for migration in MIGRATIONS {
        conn.execute_batch(migration)?;
    }

    Ok(conn)
}
