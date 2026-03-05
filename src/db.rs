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
    // v6: scheduled jobs for cron engine
    "CREATE TABLE IF NOT EXISTS scheduled_jobs (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        schedule_type TEXT NOT NULL,
        schedule_data TEXT NOT NULL,
        ghost TEXT,
        prompt TEXT NOT NULL,
        target TEXT NOT NULL DEFAULT 'broadcast',
        enabled INTEGER NOT NULL DEFAULT 1,
        next_run TEXT,
        last_run TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_jobs_enabled_next ON scheduled_jobs(enabled, next_run);",
    // v7: relationship stats for tracking user interaction patterns
    "CREATE TABLE IF NOT EXISTS relationship_stats (
        user_id TEXT PRIMARY KEY,
        total_interactions INTEGER NOT NULL DEFAULT 0,
        last_interaction TEXT NOT NULL DEFAULT (datetime('now')),
        avg_message_length REAL NOT NULL DEFAULT 0.0,
        topics_json TEXT NOT NULL DEFAULT '[]',
        sentiment_avg REAL NOT NULL DEFAULT 0.0,
        warmth_level REAL NOT NULL DEFAULT 0.5
    );",
    // v8: mood state singleton for persistence across restarts
    "CREATE TABLE IF NOT EXISTS mood_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        energy REAL NOT NULL DEFAULT 0.7,
        valence REAL NOT NULL DEFAULT 0.0,
        active_modifier TEXT NOT NULL DEFAULT 'calm',
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    INSERT OR IGNORE INTO mood_state (id) VALUES (1);",
    // v9: tool usage tracking — one row per tool, aggregated stats
    "CREATE TABLE IF NOT EXISTS tool_usage (
        tool_name TEXT PRIMARY KEY,
        invocation_count INTEGER NOT NULL DEFAULT 0,
        success_count INTEGER NOT NULL DEFAULT 0,
        failure_count INTEGER NOT NULL DEFAULT 0,
        last_used TEXT,
        avg_duration_ms REAL NOT NULL DEFAULT 0.0,
        last_error TEXT,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );",
    // v10: mission KPI snapshots (lane/repo/risk segmented trend history)
    "CREATE TABLE IF NOT EXISTS kpi_snapshots (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        lane TEXT NOT NULL,
        repo TEXT NOT NULL,
        risk_tier TEXT NOT NULL,
        captured_at TEXT NOT NULL DEFAULT (datetime('now')),
        task_success_rate REAL NOT NULL,
        verification_pass_rate REAL NOT NULL,
        rollback_rate REAL NOT NULL,
        mean_time_to_fix_secs REAL,
        tasks_started INTEGER NOT NULL,
        tasks_succeeded INTEGER NOT NULL,
        tasks_failed INTEGER NOT NULL,
        verifications_total INTEGER NOT NULL,
        verifications_passed INTEGER NOT NULL,
        rollbacks INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_kpi_snapshots_lane_repo_time
        ON kpi_snapshots(lane, repo, captured_at DESC);",
    // v11: autonomous task outcomes with lane/risk tagging for KPI attribution
    "CREATE TABLE IF NOT EXISTS autonomous_task_outcomes (
        task_id TEXT PRIMARY KEY,
        lane TEXT NOT NULL,
        repo TEXT NOT NULL,
        risk_tier TEXT NOT NULL,
        ghost TEXT,
        goal TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'started',
        started_at TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at TEXT,
        verification_total INTEGER NOT NULL DEFAULT 0,
        verification_passed INTEGER NOT NULL DEFAULT 0,
        rolled_back INTEGER NOT NULL DEFAULT 0,
        error TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_task_outcomes_lane_repo_risk_time
        ON autonomous_task_outcomes(lane, repo, risk_tier, started_at DESC);",
    // v12: ticket intake deduplication log
    "CREATE TABLE IF NOT EXISTS ticket_intake_log (
        dedup_key TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        external_id TEXT NOT NULL,
        title TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'dispatched',
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_ticket_intake_provider
        ON ticket_intake_log(provider, created_at DESC);",
    // v13: add issue_number for webhook/write-back support
    "ALTER TABLE ticket_intake_log ADD COLUMN issue_number TEXT;",
    // v14: persist CI monitor status for ticket write-back chain
    "ALTER TABLE ticket_intake_log ADD COLUMN ci_monitor_status TEXT;",
    // v15: record selected coding CLI for tool-level routing KPIs
    "ALTER TABLE autonomous_task_outcomes ADD COLUMN cli_tool_used TEXT;",
    // v16: session activity log for review & explainability
    "CREATE TABLE IF NOT EXISTS session_activity_log (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_key TEXT NOT NULL,
        event_type TEXT NOT NULL,
        summary TEXT NOT NULL,
        detail TEXT,
        ghost TEXT,
        tool_name TEXT,
        task_id TEXT,
        duration_ms INTEGER,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_session_activity_session_time
        ON session_activity_log(session_key, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_session_activity_type_time
        ON session_activity_log(event_type, created_at DESC);",
    // v17: tool detail capture + execution trees + alert rules for session review
    "ALTER TABLE session_activity_log ADD COLUMN tool_input TEXT;
    ALTER TABLE session_activity_log ADD COLUMN tool_output TEXT;
    ALTER TABLE session_activity_log ADD COLUMN parent_id INTEGER REFERENCES session_activity_log(id);
    CREATE INDEX IF NOT EXISTS idx_session_activity_parent
        ON session_activity_log(parent_id);
    CREATE TABLE IF NOT EXISTS review_alert_rules (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        pattern TEXT NOT NULL,
        target TEXT NOT NULL DEFAULT 'tool_name',
        severity TEXT NOT NULL DEFAULT 'warn',
        enabled INTEGER NOT NULL DEFAULT 1,
        chat_id TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_alert_rules_enabled
        ON review_alert_rules(enabled);",
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
            if upper.starts_with("INSERT")
                || upper.starts_with("UPDATE")
                || upper.starts_with("DELETE")
            {
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
