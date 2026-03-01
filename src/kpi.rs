use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::config::Config;
use crate::error::{AthenaError, Result};
use crate::langfuse::LangfuseClient;

#[derive(Debug, Clone, Serialize)]
pub struct KpiSnapshot {
    pub lane: String,
    pub repo: String,
    pub risk_tier: String,
    pub captured_at: String,
    pub task_success_rate: f64,
    pub verification_pass_rate: f64,
    pub rollback_rate: f64,
    pub mean_time_to_fix_secs: Option<f64>,
    pub tasks_started: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub verifications_total: u64,
    pub verifications_passed: u64,
    pub rollbacks: u64,
}

pub struct TaskOutcomeStore {
    conn: Mutex<Connection>,
}

impl TaskOutcomeStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    pub fn record_start(
        &self,
        task_id: &str,
        lane: &str,
        repo: &str,
        risk_tier: &str,
        ghost: Option<&str>,
        goal: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock task outcome store: {}", e)))?;
        conn.execute(
            "INSERT OR REPLACE INTO autonomous_task_outcomes (
                task_id, lane, repo, risk_tier, ghost, goal, status, started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'started', datetime('now'))",
            params![task_id, lane, repo, risk_tier, ghost, goal],
        )?;
        Ok(())
    }

    pub fn record_finish(
        &self,
        task_id: &str,
        status: &str,
        verification_total: u64,
        verification_passed: u64,
        rolled_back: bool,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock task outcome store: {}", e)))?;
        conn.execute(
            "UPDATE autonomous_task_outcomes
             SET status = ?2,
                 finished_at = datetime('now'),
                 verification_total = ?3,
                 verification_passed = ?4,
                 rolled_back = ?5,
                 error = ?6
             WHERE task_id = ?1",
            params![
                task_id,
                status,
                verification_total as i64,
                verification_passed as i64,
                if rolled_back { 1 } else { 0 },
                error
            ],
        )?;
        Ok(())
    }

    pub fn fail_task_if_started(&self, task_id: &str, error: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock task outcome store: {}", e)))?;
        let updated = conn.execute(
            "UPDATE autonomous_task_outcomes
             SET status = 'failed',
                 finished_at = datetime('now'),
                 error = COALESCE(error, ?2)
             WHERE task_id = ?1
               AND status = 'started'",
            params![task_id, error],
        )?;
        Ok(updated > 0)
    }

    pub fn fail_stale_started_tasks(&self, stale_after_secs: u64, error: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock task outcome store: {}", e)))?;
        let cutoff = format!("-{} seconds", stale_after_secs.max(1));
        let updated = conn.execute(
            "UPDATE autonomous_task_outcomes
             SET status = 'failed',
                 finished_at = datetime('now'),
                 error = COALESCE(error, ?2)
             WHERE status = 'started'
               AND started_at <= datetime('now', ?1)",
            params![cutoff, error],
        )?;
        Ok(updated)
    }

    pub fn update_ticket_ci_monitor_status(
        &self,
        dedup_key: &str,
        ci_monitor_status: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AthenaError::Tool(format!("Failed to lock task outcome store: {}", e)))?;
        conn.execute(
            "UPDATE ticket_intake_log
             SET ci_monitor_status = ?2
             WHERE dedup_key = ?1",
            params![dedup_key, ci_monitor_status],
        )?;
        Ok(())
    }
}

fn parse_sqlite_datetime(ts: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(DateTime::from_naive_utc_and_offset(naive, Utc))
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn count_memories(conn: &Connection, categories: &[&str]) -> Result<u64> {
    let placeholders = std::iter::repeat_n("?", categories.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT COUNT(*) FROM memories WHERE active = 1 AND category IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let count: i64 = stmt.query_row(
        rusqlite::params_from_iter(categories.iter().copied()),
        |row| row.get(0),
    )?;
    Ok(count.max(0) as u64)
}

fn verification_totals(conn: &Connection) -> Result<(u64, u64)> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(SUM(invocation_count), 0), COALESCE(SUM(success_count), 0)
         FROM tool_usage
         WHERE tool_name IN ('lint', 'test_runner')",
    )?;
    let (total, passed): (i64, i64) = stmt.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok((total.max(0) as u64, passed.max(0) as u64))
}

fn mean_time_to_fix_secs(conn: &Connection) -> Result<Option<f64>> {
    let mut fail_stmt = conn.prepare(
        "SELECT created_at FROM memories
         WHERE active = 1 AND category = 'code_change_failed'
         ORDER BY created_at ASC",
    )?;
    let mut ok_stmt = conn.prepare(
        "SELECT created_at FROM memories
         WHERE active = 1 AND category = 'code_change'
         ORDER BY created_at ASC",
    )?;

    let failures: Vec<DateTime<Utc>> = fail_stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok().and_then(|ts| parse_sqlite_datetime(&ts)))
        .collect();
    let successes: Vec<DateTime<Utc>> = ok_stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok().and_then(|ts| parse_sqlite_datetime(&ts)))
        .collect();

    if failures.is_empty() || successes.is_empty() {
        return Ok(None);
    }

    let mut success_idx = 0usize;
    let mut pairs = 0u64;
    let mut total_secs = 0f64;

    for failed_at in failures {
        while success_idx < successes.len() && successes[success_idx] <= failed_at {
            success_idx += 1;
        }
        if success_idx >= successes.len() {
            break;
        }
        let delta = successes[success_idx]
            .signed_duration_since(failed_at)
            .num_seconds();
        if delta > 0 {
            total_secs += delta as f64;
            pairs += 1;
        }
    }

    if pairs == 0 {
        Ok(None)
    } else {
        Ok(Some(total_secs / pairs as f64))
    }
}

fn mean_time_to_fix_from_outcomes(
    conn: &Connection,
    lane: &str,
    repo: &str,
    risk_tier: &str,
) -> Result<Option<f64>> {
    let mut fail_stmt = conn.prepare(
        "SELECT finished_at FROM autonomous_task_outcomes
         WHERE lane = ?1 AND repo = ?2 AND risk_tier = ?3
           AND status = 'failed' AND finished_at IS NOT NULL
         ORDER BY finished_at ASC",
    )?;
    let mut ok_stmt = conn.prepare(
        "SELECT finished_at FROM autonomous_task_outcomes
         WHERE lane = ?1 AND repo = ?2 AND risk_tier = ?3
           AND status = 'succeeded' AND finished_at IS NOT NULL
         ORDER BY finished_at ASC",
    )?;
    let failures: Vec<DateTime<Utc>> = fail_stmt
        .query_map(params![lane, repo, risk_tier], |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok().and_then(|ts| parse_sqlite_datetime(&ts)))
        .collect();
    let successes: Vec<DateTime<Utc>> = ok_stmt
        .query_map(params![lane, repo, risk_tier], |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok().and_then(|ts| parse_sqlite_datetime(&ts)))
        .collect();
    if failures.is_empty() || successes.is_empty() {
        return Ok(None);
    }
    let mut success_idx = 0usize;
    let mut pairs = 0u64;
    let mut total_secs = 0f64;
    for failed_at in failures {
        while success_idx < successes.len() && successes[success_idx] <= failed_at {
            success_idx += 1;
        }
        if success_idx >= successes.len() {
            break;
        }
        let delta = successes[success_idx]
            .signed_duration_since(failed_at)
            .num_seconds();
        if delta > 0 {
            total_secs += delta as f64;
            pairs += 1;
        }
    }
    if pairs == 0 {
        Ok(None)
    } else {
        Ok(Some(total_secs / pairs as f64))
    }
}

pub fn default_repo_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn open_connection(config: &Config) -> Result<Connection> {
    let db_path: PathBuf = config.db_path()?;
    let conn = Connection::open(&db_path)?;
    Ok(conn)
}

pub fn compute_snapshot(
    conn: &Connection,
    lane: &str,
    repo: &str,
    risk_tier: &str,
) -> Result<KpiSnapshot> {
    if let Some(tagged) = compute_snapshot_from_tagged_outcomes(conn, lane, repo, risk_tier)? {
        return Ok(tagged);
    }

    let tasks_succeeded = count_memories(conn, &["code_change"])?;
    let tasks_failed = count_memories(
        conn,
        &[
            "code_change_failed",
            "refactoring_failed",
            "improvement_idea_failed",
        ],
    )?;
    let tasks_started = tasks_succeeded + tasks_failed;

    let (verifications_total, verifications_passed) = verification_totals(conn)?;
    let rollbacks = count_memories(conn, &["code_change_rollback", "rollback"])?;
    let mttr = mean_time_to_fix_secs(conn)?;

    Ok(KpiSnapshot {
        lane: lane.to_string(),
        repo: repo.to_string(),
        risk_tier: risk_tier.to_string(),
        captured_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        task_success_rate: ratio(tasks_succeeded, tasks_started),
        verification_pass_rate: ratio(verifications_passed, verifications_total),
        rollback_rate: ratio(rollbacks, tasks_succeeded),
        mean_time_to_fix_secs: mttr,
        tasks_started,
        tasks_succeeded,
        tasks_failed,
        verifications_total,
        verifications_passed,
        rollbacks,
    })
}

fn compute_snapshot_from_tagged_outcomes(
    conn: &Connection,
    lane: &str,
    repo: &str,
    risk_tier: &str,
) -> Result<Option<KpiSnapshot>> {
    if !table_exists(conn, "autonomous_task_outcomes")? {
        return Ok(None);
    }
    let (tasks_started, tasks_succeeded, tasks_failed): (i64, i64, i64) = conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN status IN ('succeeded', 'failed', 'rolled_back') THEN 1 ELSE 0 END), 0) as started,
            COALESCE(SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END), 0) as succeeded,
            COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) as failed
         FROM autonomous_task_outcomes
         WHERE lane = ?1 AND repo = ?2 AND risk_tier = ?3",
        params![lane, repo, risk_tier],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if tasks_started <= 0 {
        return Ok(None);
    }

    let (verifications_total, verifications_passed, rollbacks): (i64, i64, i64) = conn.query_row(
        "SELECT
            COALESCE(SUM(verification_total), 0),
            COALESCE(SUM(verification_passed), 0),
            COALESCE(SUM(rolled_back), 0)
         FROM autonomous_task_outcomes
         WHERE lane = ?1 AND repo = ?2 AND risk_tier = ?3",
        params![lane, repo, risk_tier],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let started = tasks_started.max(0) as u64;
    let succeeded = tasks_succeeded.max(0) as u64;
    let failed = tasks_failed.max(0) as u64;
    let ver_total = verifications_total.max(0) as u64;
    let ver_passed = verifications_passed.max(0) as u64;
    let rollbacks = rollbacks.max(0) as u64;
    let mttr = mean_time_to_fix_from_outcomes(conn, lane, repo, risk_tier)?;

    Ok(Some(KpiSnapshot {
        lane: lane.to_string(),
        repo: repo.to_string(),
        risk_tier: risk_tier.to_string(),
        captured_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        task_success_rate: ratio(succeeded, started),
        verification_pass_rate: ratio(ver_passed, ver_total),
        rollback_rate: ratio(rollbacks, succeeded),
        mean_time_to_fix_secs: mttr,
        tasks_started: started,
        tasks_succeeded: succeeded,
        tasks_failed: failed,
        verifications_total: ver_total,
        verifications_passed: ver_passed,
        rollbacks,
    }))
}

pub fn store_snapshot(conn: &Connection, snapshot: &KpiSnapshot) -> Result<()> {
    conn.execute(
        "INSERT INTO kpi_snapshots (
            lane, repo, risk_tier, captured_at,
            task_success_rate, verification_pass_rate, rollback_rate, mean_time_to_fix_secs,
            tasks_started, tasks_succeeded, tasks_failed,
            verifications_total, verifications_passed, rollbacks
        ) VALUES (?1, ?2, ?3, datetime('now'), ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            snapshot.lane,
            snapshot.repo,
            snapshot.risk_tier,
            snapshot.task_success_rate,
            snapshot.verification_pass_rate,
            snapshot.rollback_rate,
            snapshot.mean_time_to_fix_secs,
            snapshot.tasks_started as i64,
            snapshot.tasks_succeeded as i64,
            snapshot.tasks_failed as i64,
            snapshot.verifications_total as i64,
            snapshot.verifications_passed as i64,
            snapshot.rollbacks as i64,
        ],
    )?;
    Ok(())
}

pub fn list_history(
    conn: &Connection,
    lane: Option<&str>,
    repo: Option<&str>,
    limit: usize,
) -> Result<Vec<KpiSnapshot>> {
    let mut sql = String::from(
        "SELECT lane, repo, risk_tier, captured_at,
                task_success_rate, verification_pass_rate, rollback_rate, mean_time_to_fix_secs,
                tasks_started, tasks_succeeded, tasks_failed,
                verifications_total, verifications_passed, rollbacks
         FROM kpi_snapshots",
    );
    let mut where_parts: Vec<&str> = Vec::new();
    if lane.is_some() {
        where_parts.push("lane = ?");
    }
    if repo.is_some() {
        where_parts.push("repo = ?");
    }
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }
    sql.push_str(
        " ORDER BY datetime(replace(replace(captured_at, 'T', ' '), 'Z', '')) DESC, captured_at DESC LIMIT ?",
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut params_dyn: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = lane {
        params_dyn.push(v.to_string().into());
    }
    if let Some(v) = repo {
        params_dyn.push(v.to_string().into());
    }
    params_dyn.push((limit as i64).into());

    let rows = stmt.query_map(rusqlite::params_from_iter(params_dyn), |row| {
        Ok(KpiSnapshot {
            lane: row.get(0)?,
            repo: row.get(1)?,
            risk_tier: row.get(2)?,
            captured_at: row.get(3)?,
            task_success_rate: row.get(4)?,
            verification_pass_rate: row.get(5)?,
            rollback_rate: row.get(6)?,
            mean_time_to_fix_secs: row.get(7)?,
            tasks_started: row.get::<_, i64>(8)?.max(0) as u64,
            tasks_succeeded: row.get::<_, i64>(9)?.max(0) as u64,
            tasks_failed: row.get::<_, i64>(10)?.max(0) as u64,
            verifications_total: row.get::<_, i64>(11)?.max(0) as u64,
            verifications_passed: row.get::<_, i64>(12)?.max(0) as u64,
            rollbacks: row.get::<_, i64>(13)?.max(0) as u64,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn print_snapshot(snapshot: &KpiSnapshot) {
    println!("KPI Snapshot");
    println!(
        "lane={} repo={} risk={} captured_at={}",
        snapshot.lane, snapshot.repo, snapshot.risk_tier, snapshot.captured_at
    );
    println!(
        "task_success_rate={:.2}% ({}/{})",
        snapshot.task_success_rate * 100.0,
        snapshot.tasks_succeeded,
        snapshot.tasks_started
    );
    println!(
        "verification_pass_rate={:.2}% ({}/{})",
        snapshot.verification_pass_rate * 100.0,
        snapshot.verifications_passed,
        snapshot.verifications_total
    );
    println!(
        "rollback_rate={:.2}% ({}/{})",
        snapshot.rollback_rate * 100.0,
        snapshot.rollbacks,
        snapshot.tasks_succeeded
    );
    match snapshot.mean_time_to_fix_secs {
        Some(v) => println!("mean_time_to_fix={:.1}h", v / 3600.0),
        None => println!("mean_time_to_fix=n/a"),
    }
}

pub fn print_history(rows: &[KpiSnapshot]) {
    if rows.is_empty() {
        println!("No KPI snapshots recorded yet.");
        return;
    }
    println!("KPI History (latest first)");
    for s in rows {
        let mttr = s
            .mean_time_to_fix_secs
            .map(|v| format!("{:.1}h", v / 3600.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!(
            "{} lane={} repo={} risk={} success={:.1}% verify={:.1}% rollback={:.1}% mttr={}",
            s.captured_at,
            s.lane,
            s.repo,
            s.risk_tier,
            s.task_success_rate * 100.0,
            s.verification_pass_rate * 100.0,
            s.rollback_rate * 100.0,
            mttr
        );
    }
}

fn langfuse_client_from_config(config: &Config) -> Option<Arc<LangfuseClient>> {
    let public_key = config
        .langfuse
        .public_key
        .clone()
        .or_else(|| std::env::var("LANGFUSE_PUBLIC_KEY").ok())?;
    let secret_key = config
        .langfuse
        .secret_key
        .clone()
        .or_else(|| std::env::var("LANGFUSE_SECRET_KEY").ok())?;
    Some(Arc::new(LangfuseClient::new(
        public_key,
        secret_key,
        config.langfuse.base_url.clone(),
    )))
}

pub async fn emit_snapshot_to_langfuse(config: &Config, snapshot: &KpiSnapshot) -> Result<()> {
    let Some(client) = langfuse_client_from_config(config) else {
        return Err(AthenaError::Config(
            "Langfuse credentials missing; set LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY."
                .to_string(),
        ));
    };
    let payload = serde_json::to_value(snapshot).unwrap_or_default();
    client
        .emit_kpi_snapshot(&snapshot.lane, &snapshot.repo, &snapshot.risk_tier, payload)
        .await
        .map_err(AthenaError::Tool)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                category TEXT NOT NULL,
                content TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                embedding BLOB
            );
            CREATE TABLE tool_usage (
                tool_name TEXT PRIMARY KEY,
                invocation_count INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                last_used TEXT,
                avg_duration_ms REAL NOT NULL DEFAULT 0.0,
                last_error TEXT,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE kpi_snapshots (
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
            CREATE TABLE autonomous_task_outcomes (
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
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn computes_rates_from_memory_and_tools() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO memories (id, category, content, created_at) VALUES
             ('1','code_change','ok','2026-01-01 10:00:00'),
             ('2','code_change','ok2','2026-01-01 12:00:00'),
             ('3','code_change_failed','bad','2026-01-01 11:00:00'),
             ('4','rollback','rb','2026-01-02 11:00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_usage (tool_name, invocation_count, success_count, failure_count)
             VALUES ('lint', 4, 3, 1), ('test_runner', 2, 1, 1)",
            [],
        )
        .unwrap();

        let snap = compute_snapshot(&conn, "delivery", "athena", "medium").unwrap();
        assert_eq!(snap.tasks_started, 3);
        assert_eq!(snap.tasks_succeeded, 2);
        assert_eq!(snap.tasks_failed, 1);
        assert!((snap.task_success_rate - (2.0 / 3.0)).abs() < 1e-6);
        assert_eq!(snap.verifications_total, 6);
        assert_eq!(snap.verifications_passed, 4);
        assert!((snap.verification_pass_rate - (4.0 / 6.0)).abs() < 1e-6);
        assert_eq!(snap.rollbacks, 1);
        assert!((snap.rollback_rate - 0.5).abs() < 1e-6);
    }

    #[test]
    fn stores_and_lists_snapshots() {
        let conn = setup_conn();
        let snap = KpiSnapshot {
            lane: "self_improvement".to_string(),
            repo: "athena".to_string(),
            risk_tier: "medium".to_string(),
            captured_at: "2026-01-01T00:00:00.000Z".to_string(),
            task_success_rate: 0.5,
            verification_pass_rate: 0.7,
            rollback_rate: 0.1,
            mean_time_to_fix_secs: Some(3600.0),
            tasks_started: 10,
            tasks_succeeded: 5,
            tasks_failed: 5,
            verifications_total: 10,
            verifications_passed: 7,
            rollbacks: 1,
        };
        store_snapshot(&conn, &snap).unwrap();
        let rows = list_history(&conn, Some("self_improvement"), Some("athena"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lane, "self_improvement");
    }

    #[test]
    fn list_history_orders_mixed_timestamp_formats_descending() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO kpi_snapshots
             (lane, repo, risk_tier, captured_at, task_success_rate, verification_pass_rate, rollback_rate, mean_time_to_fix_secs,
              tasks_started, tasks_succeeded, tasks_failed, verifications_total, verifications_passed, rollbacks)
             VALUES
             ('delivery','athena','low','2026-02-17T05:28:37.000Z',0.11,1.0,0.0,NULL,10,1,9,1,1,0),
             ('delivery','athena','low','2026-02-17 18:24:17',0.40,1.0,0.0,NULL,10,4,6,1,1,0)",
            [],
        )
        .unwrap();

        let rows = list_history(&conn, Some("delivery"), Some("athena"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].captured_at, "2026-02-17 18:24:17");
        assert_eq!(rows[1].captured_at, "2026-02-17T05:28:37.000Z");
    }

    #[test]
    fn prefers_tagged_outcomes_for_lane_metrics() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO autonomous_task_outcomes
             (task_id, lane, repo, risk_tier, goal, status, started_at, finished_at, verification_total, verification_passed, rolled_back)
             VALUES
             ('a1','delivery','athena','high','task1','succeeded','2026-01-01 10:00:00','2026-01-01 10:10:00',1,1,0),
             ('a2','delivery','athena','high','task2','failed','2026-01-01 11:00:00','2026-01-01 11:10:00',1,0,0),
             ('a3','delivery','athena','high','task3','succeeded','2026-01-01 12:00:00','2026-01-01 12:20:00',1,1,1)",
            [],
        )
        .unwrap();

        let snap = compute_snapshot(&conn, "delivery", "athena", "high").unwrap();
        assert_eq!(snap.tasks_started, 3);
        assert_eq!(snap.tasks_succeeded, 2);
        assert_eq!(snap.tasks_failed, 1);
        assert!((snap.task_success_rate - (2.0 / 3.0)).abs() < 1e-6);
        assert_eq!(snap.verifications_total, 3);
        assert_eq!(snap.verifications_passed, 2);
        assert!((snap.verification_pass_rate - (2.0 / 3.0)).abs() < 1e-6);
        assert_eq!(snap.rollbacks, 1);
        assert!((snap.rollback_rate - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fail_task_if_started_only_updates_started_rows() {
        let conn = setup_conn();
        let store = TaskOutcomeStore::new(conn);
        store
            .record_start("t1", "delivery", "athena", "low", Some("coder"), "goal")
            .unwrap();
        store
            .record_start("t2", "delivery", "athena", "low", Some("coder"), "goal")
            .unwrap();
        store
            .record_finish("t2", "succeeded", 0, 0, false, None)
            .unwrap();

        assert!(store
            .fail_task_if_started("t1", "dispatch wait timeout")
            .unwrap());
        assert!(!store
            .fail_task_if_started("t2", "dispatch wait timeout")
            .unwrap());

        let conn = store.conn.lock().unwrap();
        let t1: (String, Option<String>) = conn
            .query_row(
                "SELECT status, error FROM autonomous_task_outcomes WHERE task_id='t1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let t2: String = conn
            .query_row(
                "SELECT status FROM autonomous_task_outcomes WHERE task_id='t2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t1.0, "failed");
        assert_eq!(t1.1.as_deref(), Some("dispatch wait timeout"));
        assert_eq!(t2, "succeeded");
    }

    #[test]
    fn fail_stale_started_tasks_marks_only_old_rows() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO autonomous_task_outcomes
             (task_id, lane, repo, risk_tier, goal, status, started_at)
             VALUES
             ('old-started','delivery','athena','low','goal','started',datetime('now','-7200 seconds')),
             ('fresh-started','delivery','athena','low','goal','started',datetime('now','-60 seconds')),
             ('already-done','delivery','athena','low','goal','succeeded',datetime('now','-7200 seconds'))",
            [],
        )
        .unwrap();
        let store = TaskOutcomeStore::new(conn);

        let changed = store
            .fail_stale_started_tasks(1800, "stale_started")
            .unwrap();
        assert_eq!(changed, 1);

        let conn = store.conn.lock().unwrap();
        let old: String = conn
            .query_row(
                "SELECT status FROM autonomous_task_outcomes WHERE task_id='old-started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let fresh: String = conn
            .query_row(
                "SELECT status FROM autonomous_task_outcomes WHERE task_id='fresh-started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let done: String = conn
            .query_row(
                "SELECT status FROM autonomous_task_outcomes WHERE task_id='already-done'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, "failed");
        assert_eq!(fresh, "started");
        assert_eq!(done, "succeeded");
    }
}
