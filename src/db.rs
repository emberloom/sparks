use rusqlite::Connection;
use std::path::Path;

use crate::error::{AthenaError, Result};

const MIGRATIONS: &[&str] = &[
    // v1: memories table + schema version tracking
    "CREATE TABLE IF NOT EXISTS memories (
        id TEXT PRIMARY KEY,
        category TEXT NOT NULL,
        content TEXT NOT NULL,
        active INTEGER NOT NULL DEFAULT 1,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY
    );",
    // v2: embedding column for vector search
    "ALTER TABLE memories ADD COLUMN embedding BLOB;",
    // v3: FTS5 full-text search index (standalone, not external-content)
    "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(content);
     INSERT INTO memories_fts(rowid, content) SELECT rowid, content FROM memories WHERE active = 1;",
    // v4: conversation history for multi-turn context
    "CREATE TABLE IF NOT EXISTS conversations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_key TEXT NOT NULL,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_conversations_session ON conversations(session_key, created_at);",
    // v5: user profiles — key-value store per user for cross-session context
    "CREATE TABLE IF NOT EXISTS user_profiles (
        user_id TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (user_id, key)
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
    let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;

    run_migrations(&conn)?;

    Ok(conn)
}

fn current_version(conn: &Connection) -> i64 {
    // schema_version table might not exist yet
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

fn run_migrations(conn: &Connection) -> Result<()> {
    // First, ensure at least the base tables exist so we can query schema_version.
    // If this is a fresh DB, run migration 0 unconditionally.
    let has_schema_table: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !has_schema_table {
        // Fresh database — run the first migration to bootstrap
        conn.execute_batch(MIGRATIONS[0])?;
        conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [1i64])?;
    }

    let current = current_version(conn);

    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        tracing::info!("Running database migration v{}", version);
        // Run each statement individually — execute_batch chokes on
        // statements that return results (e.g. INSERT...SELECT).
        for stmt in migration.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            tracing::debug!("  Running: {}", &stmt[..stmt.len().min(80)]);
            // Use execute() for DML (INSERT/UPDATE/DELETE) since execute_batch
            // fails on statements that return results.
            let upper = stmt.to_uppercase();
            if upper.starts_with("INSERT") || upper.starts_with("UPDATE") || upper.starts_with("DELETE") {
                conn.execute(stmt, [])?;
            } else {
                conn.execute_batch(&format!("{};", stmt))?;
            }
        }
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
    }

    Ok(())
}
