use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{AthenaError, Result};

#[cfg(any(feature = "telegram", feature = "slack"))]
use crate::llm::{ChatRole, LlmProvider, Message};
#[cfg(any(feature = "telegram", feature = "slack"))]
use serde::Serialize;

// ── Event types for the activity log ────────────────────────────────

/// Categories of session events that get logged for review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityEventType {
    ChatIn,
    ChatOut,
    ToolRun,
    AutonomousTaskStart,
    AutonomousTaskFinish,
    AutonomousTaskFail,
}

impl ActivityEventType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ChatIn => "chat_in",
            Self::ChatOut => "chat_out",
            Self::ToolRun => "tool_run",
            Self::AutonomousTaskStart => "task_start",
            Self::AutonomousTaskFinish => "task_finish",
            Self::AutonomousTaskFail => "task_fail",
        }
    }
}

// ── Stored activity entry ───────────────────────────────────────────

#[cfg(any(feature = "telegram", feature = "slack"))]
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub id: i64,
    pub session_key: String,
    pub event_type: String,
    pub summary: String,
    pub detail: Option<String>,
    pub ghost: Option<String>,
    pub tool_name: Option<String>,
    pub task_id: Option<String>,
    pub duration_ms: Option<i64>,
    pub tool_input: Option<String>,
    pub tool_output: Option<String>,
    pub parent_id: Option<i64>,
    pub created_at: String,
}

// ── Alert rules ─────────────────────────────────────────────────────

#[cfg(any(feature = "telegram", feature = "slack"))]
#[derive(Debug, Clone, Serialize)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub target: String,
    pub severity: String,
    pub enabled: bool,
    pub chat_id: Option<String>,
}

#[cfg(any(feature = "telegram", feature = "slack"))]
#[derive(Debug, Clone, Serialize)]
pub struct AlertMatch {
    pub rule: AlertRule,
    pub entry: ActivityEntry,
}

// ── Detail levels for review rendering ──────────────────────────────

#[cfg(any(feature = "telegram", feature = "slack"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDetail {
    /// One-paragraph executive summary.
    Summary,
    /// Timeline with key events, tools used, outcomes.
    Standard,
    /// Full detail: every event, reasoning, tool outputs.
    Detailed,
}

#[cfg(any(feature = "telegram", feature = "slack"))]
impl ReviewDetail {
    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().trim() {
            "summary" | "brief" | "tldr" => Self::Summary,
            "detailed" | "full" | "verbose" => Self::Detailed,
            _ => Self::Standard,
        }
    }
}

// ── Activity log store ──────────────────────────────────────────────

/// SQLite-backed store for session activity logs.
pub struct ActivityLogStore {
    conn: Mutex<Connection>,
}

fn lock_conn(conn: &Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>> {
    conn.lock()
        .map_err(|e| AthenaError::Tool(format!("Failed to lock activity log store: {}", e)))
}

/// Helper to build an ActivityEntry from a row that selects all 13 columns.
#[cfg(any(feature = "telegram", feature = "slack"))]
fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActivityEntry> {
    Ok(ActivityEntry {
        id: row.get(0)?,
        session_key: row.get(1)?,
        event_type: row.get(2)?,
        summary: row.get(3)?,
        detail: row.get(4)?,
        ghost: row.get(5)?,
        tool_name: row.get(6)?,
        task_id: row.get(7)?,
        duration_ms: row.get(8)?,
        tool_input: row.get(9)?,
        tool_output: row.get(10)?,
        parent_id: row.get(11)?,
        created_at: row.get(12)?,
    })
}

#[cfg(any(feature = "telegram", feature = "slack"))]
const SELECT_COLS: &str =
    "id, session_key, event_type, summary, detail, ghost, tool_name, task_id, \
     duration_ms, tool_input, tool_output, parent_id, created_at";

impl ActivityLogStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Record a session activity event. Returns the inserted row id.
    pub fn record(
        &self,
        session_key: &str,
        event_type: ActivityEventType,
        summary: &str,
        detail: Option<&str>,
        ghost: Option<&str>,
        tool_name: Option<&str>,
        task_id: Option<&str>,
        duration_ms: Option<i64>,
    ) -> Result<i64> {
        let conn = lock_conn(&self.conn)?;
        conn.execute(
            "INSERT INTO session_activity_log
             (session_key, event_type, summary, detail, ghost, tool_name, task_id, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                session_key,
                event_type.label(),
                summary,
                detail,
                ghost,
                tool_name,
                task_id,
                duration_ms,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Record a tool execution with full input/output capture.
    pub fn record_tool(
        &self,
        session_key: &str,
        tool_name: &str,
        summary: &str,
        tool_input: Option<&str>,
        tool_output: Option<&str>,
        ghost: Option<&str>,
        task_id: Option<&str>,
        duration_ms: Option<i64>,
        parent_id: Option<i64>,
    ) -> Result<i64> {
        let conn = lock_conn(&self.conn)?;
        conn.execute(
            "INSERT INTO session_activity_log
             (session_key, event_type, summary, tool_name, tool_input, tool_output,
              ghost, task_id, duration_ms, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                session_key,
                ActivityEventType::ToolRun.label(),
                summary,
                tool_name,
                tool_input,
                tool_output,
                ghost,
                task_id,
                duration_ms,
                parent_id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get recent activity entries for a session, newest first.
    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn recent(&self, session_key: &str, limit: usize) -> Result<Vec<ActivityEntry>> {
        let conn = lock_conn(&self.conn)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM session_activity_log
             WHERE session_key = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
            SELECT_COLS
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![session_key, limit as i64],
            entry_from_row,
        )?;
        let mut entries: Vec<ActivityEntry> = rows.filter_map(|r| r.ok()).collect();
        entries.reverse(); // chronological order
        Ok(entries)
    }

    /// Search across all sessions for entries matching a text pattern.
    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ActivityEntry>> {
        let conn = lock_conn(&self.conn)?;
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM session_activity_log
             WHERE summary LIKE ?1
                OR detail LIKE ?1
                OR tool_name LIKE ?1
                OR tool_input LIKE ?1
                OR tool_output LIKE ?1
                OR ghost LIKE ?1
             ORDER BY created_at DESC
             LIMIT ?2",
            SELECT_COLS
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![pattern, limit as i64],
            entry_from_row,
        )?;
        let mut entries: Vec<ActivityEntry> = rows.filter_map(|r| r.ok()).collect();
        entries.reverse();
        Ok(entries)
    }

    // ── Alert rule management ───────────────────────────────────────

    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn add_alert_rule(
        &self,
        name: &str,
        pattern: &str,
        target: &str,
        severity: &str,
        chat_id: Option<&str>,
    ) -> Result<i64> {
        let conn = lock_conn(&self.conn)?;
        conn.execute(
            "INSERT INTO review_alert_rules (name, pattern, target, severity, chat_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![name, pattern, target, severity, chat_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn list_alert_rules(&self) -> Result<Vec<AlertRule>> {
        let conn = lock_conn(&self.conn)?;
        let mut stmt = conn.prepare(
            "SELECT id, name, pattern, target, severity, enabled, chat_id
             FROM review_alert_rules
             ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AlertRule {
                id: row.get(0)?,
                name: row.get(1)?,
                pattern: row.get(2)?,
                target: row.get(3)?,
                severity: row.get(4)?,
                enabled: row.get::<_, i64>(5)? != 0,
                chat_id: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn remove_alert_rule(&self, rule_id: i64) -> Result<bool> {
        let conn = lock_conn(&self.conn)?;
        let deleted = conn.execute(
            "DELETE FROM review_alert_rules WHERE id = ?1",
            rusqlite::params![rule_id],
        )?;
        Ok(deleted > 0)
    }

    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn toggle_alert_rule(&self, rule_id: i64, enabled: bool) -> Result<bool> {
        let conn = lock_conn(&self.conn)?;
        let updated = conn.execute(
            "UPDATE review_alert_rules SET enabled = ?1 WHERE id = ?2",
            rusqlite::params![enabled as i64, rule_id],
        )?;
        Ok(updated > 0)
    }

    /// Check a new entry against all enabled alert rules.
    #[cfg(any(feature = "telegram", feature = "slack"))]
    pub fn check_alerts(&self, entry: &ActivityEntry) -> Result<Vec<AlertMatch>> {
        let rules = self.list_alert_rules()?;
        let mut matches = Vec::new();
        for rule in rules {
            if !rule.enabled {
                continue;
            }
            let haystack = match rule.target.as_str() {
                "tool_name" => entry.tool_name.as_deref().unwrap_or(""),
                "summary" => &entry.summary,
                "detail" => entry.detail.as_deref().unwrap_or(""),
                "tool_input" => entry.tool_input.as_deref().unwrap_or(""),
                "tool_output" => entry.tool_output.as_deref().unwrap_or(""),
                "ghost" => entry.ghost.as_deref().unwrap_or(""),
                "event_type" => &entry.event_type,
                // "any" matches against all fields
                _ => {
                    let all = format!(
                        "{} {} {} {} {} {}",
                        entry.summary,
                        entry.detail.as_deref().unwrap_or(""),
                        entry.tool_name.as_deref().unwrap_or(""),
                        entry.tool_input.as_deref().unwrap_or(""),
                        entry.tool_output.as_deref().unwrap_or(""),
                        entry.ghost.as_deref().unwrap_or(""),
                    );
                    if all.contains(&rule.pattern) {
                        matches.push(AlertMatch {
                            rule,
                            entry: entry.clone(),
                        });
                    }
                    continue;
                }
            };
            if haystack.contains(&rule.pattern) {
                matches.push(AlertMatch {
                    rule,
                    entry: entry.clone(),
                });
            }
        }
        Ok(matches)
    }
}

// ── Review rendering ────────────────────────────────────────────────

/// Render a structured review of session activity (no LLM needed).
#[cfg(any(feature = "telegram", feature = "slack"))]
pub fn render_review(entries: &[ActivityEntry], detail: ReviewDetail) -> String {
    if entries.is_empty() {
        return "No activity recorded for this session yet.".to_string();
    }

    match detail {
        ReviewDetail::Summary => render_summary(entries),
        ReviewDetail::Standard => render_standard(entries),
        ReviewDetail::Detailed => render_detailed(entries),
    }
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn render_summary(entries: &[ActivityEntry]) -> String {
    let chat_in = entries.iter().filter(|e| e.event_type == "chat_in").count();
    let chat_out = entries.iter().filter(|e| e.event_type == "chat_out").count();
    let tools = entries.iter().filter(|e| e.event_type == "tool_run").count();
    let tasks_started = entries
        .iter()
        .filter(|e| e.event_type == "task_start")
        .count();
    let tasks_ok = entries
        .iter()
        .filter(|e| e.event_type == "task_finish")
        .count();
    let tasks_fail = entries
        .iter()
        .filter(|e| e.event_type == "task_fail")
        .count();

    let total_duration_ms: i64 = entries.iter().filter_map(|e| e.duration_ms).sum();

    let time_range = if entries.len() >= 2 {
        format!(
            "{} → {}",
            &entries[0].created_at,
            &entries[entries.len() - 1].created_at
        )
    } else {
        entries[0].created_at.clone()
    };

    let unique_tools: Vec<String> = {
        let mut t: Vec<String> = entries
            .iter()
            .filter_map(|e| e.tool_name.clone())
            .collect();
        t.sort();
        t.dedup();
        t
    };

    let unique_ghosts: Vec<String> = {
        let mut g: Vec<String> = entries
            .iter()
            .filter_map(|e| e.ghost.clone())
            .collect();
        g.sort();
        g.dedup();
        g
    };

    let mut out = String::new();
    out.push_str("<b>📋 Session Summary</b>\n");
    out.push_str(&format!("⏱ {}\n\n", time_range));
    out.push_str(&format!(
        "💬 {} messages in, {} responses out\n",
        chat_in, chat_out
    ));
    out.push_str(&format!("🔧 {} tool executions\n", tools));
    if tasks_started > 0 {
        out.push_str(&format!(
            "🚀 {} tasks dispatched (✅{} ❌{})\n",
            tasks_started, tasks_ok, tasks_fail
        ));
    }
    if total_duration_ms > 0 {
        out.push_str(&format!("⏱ Total processing: {}ms\n", total_duration_ms));
    }
    if !unique_tools.is_empty() {
        out.push_str(&format!("🛠 Tools: {}\n", escape_html(&unique_tools.join(", "))));
    }
    if !unique_ghosts.is_empty() {
        out.push_str(&format!("👻 Ghosts: {}\n", escape_html(&unique_ghosts.join(", "))));
    }
    out
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn render_standard(entries: &[ActivityEntry]) -> String {
    let mut out = render_summary(entries);
    out.push_str("\n<b>📜 Timeline</b>\n");
    for entry in entries {
        let emoji = type_emoji(&entry.event_type);
        let time = entry
            .created_at
            .split_whitespace()
            .nth(1)
            .unwrap_or(&entry.created_at);
        let mut line = format!("<code>{}</code> {} {}", time, emoji, escape_html(&entry.summary));
        if let Some(ref ghost) = entry.ghost {
            line.push_str(&format!(" [{}]", escape_html(ghost)));
        }
        if let Some(ms) = entry.duration_ms {
            line.push_str(&format!(" ({}ms)", ms));
        }
        // Show tool input preview for tool_run entries
        if entry.event_type == "tool_run" {
            if let Some(ref input) = entry.tool_input {
                let preview = truncate_str(input, 80);
                line.push_str(&format!("\n  → <code>{}</code>", escape_html(&preview)));
            }
        }
        if entry.parent_id.is_some() {
            line = format!("  ↳ {}", line);
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn render_detailed(entries: &[ActivityEntry]) -> String {
    let mut out = render_summary(entries);
    out.push_str("\n<b>📜 Detailed Log</b>\n\n");
    for entry in entries {
        let emoji = type_emoji(&entry.event_type);
        let indent = if entry.parent_id.is_some() { "  ↳ " } else { "" };
        out.push_str(&format!(
            "{}<b>{} [{}]</b> {}\n",
            indent, emoji, escape_html(&entry.created_at), escape_html(&entry.summary)
        ));
        if let Some(ref ghost) = entry.ghost {
            out.push_str(&format!("{}  👻 Ghost: {}\n", indent, escape_html(ghost)));
        }
        if let Some(ref tool) = entry.tool_name {
            out.push_str(&format!("{}  🛠 Tool: {}\n", indent, escape_html(tool)));
        }
        if let Some(ref task_id) = entry.task_id {
            out.push_str(&format!("{}  🆔 Task: {}\n", indent, escape_html(task_id)));
        }
        if let Some(ms) = entry.duration_ms {
            out.push_str(&format!("{}  ⏱ Duration: {}ms\n", indent, ms));
        }
        if let Some(ref input) = entry.tool_input {
            let truncated = truncate_str(input, 300);
            out.push_str(&format!("{}  📥 Input: <code>{}</code>\n", indent, escape_html(&truncated)));
        }
        if let Some(ref output) = entry.tool_output {
            let truncated = truncate_str(output, 500);
            out.push_str(&format!("{}  📤 Output: <code>{}</code>\n", indent, escape_html(&truncated)));
        }
        if let Some(ref detail) = entry.detail {
            let truncated = truncate_str(detail, 500);
            out.push_str(&format!("{}  📝 {}\n", indent, escape_html(&truncated)));
        }
        out.push('\n');
    }
    out
}

/// Render search results.
#[cfg(any(feature = "telegram", feature = "slack"))]
pub fn render_search_results(entries: &[ActivityEntry], query: &str) -> String {
    if entries.is_empty() {
        return format!("No results found for \"{}\".", escape_html(query));
    }
    let mut out = format!(
        "<b>🔍 Search results for \"{}\"</b> ({} matches)\n\n",
        escape_html(query),
        entries.len()
    );
    for entry in entries.iter().take(30) {
        let emoji = type_emoji(&entry.event_type);
        let time = entry
            .created_at
            .split_whitespace()
            .nth(1)
            .unwrap_or(&entry.created_at);
        out.push_str(&format!(
            "<code>{}</code> {} {} [{}]\n",
            time, emoji, escape_html(&entry.summary), escape_html(&entry.session_key)
        ));
        if let Some(ref tool) = entry.tool_name {
            out.push_str(&format!("  🛠 {}", escape_html(tool)));
            if let Some(ref input) = entry.tool_input {
                out.push_str(&format!(": <code>{}</code>", escape_html(&truncate_str(input, 60))));
            }
            out.push('\n');
        }
    }
    out
}

/// Render alert rules list.
#[cfg(any(feature = "telegram", feature = "slack"))]
pub fn render_alert_rules(rules: &[AlertRule]) -> String {
    if rules.is_empty() {
        return "<b>🔔 Alert Rules</b>\n\nNo alert rules configured.\n\nUsage:\n<code>/alerts add &lt;name&gt; &lt;pattern&gt; [target] [severity]</code>\n<code>/alerts remove &lt;id&gt;</code>".to_string();
    }
    let mut out = format!("<b>🔔 Alert Rules</b> ({} total)\n\n", rules.len());
    for rule in rules {
        let status = if rule.enabled { "✅" } else { "⏸" };
        out.push_str(&format!(
            "{} <b>#{}</b> {} — <code>{}</code> on <i>{}</i> [{}]\n",
            status, rule.id, escape_html(&rule.name), escape_html(&rule.pattern),
            escape_html(&rule.target), escape_html(&rule.severity)
        ));
    }
    out.push_str("\n<code>/alerts add &lt;name&gt; &lt;pattern&gt; [target] [severity]</code>\n");
    out.push_str("<code>/alerts remove &lt;id&gt;</code>\n");
    out.push_str("<code>/alerts toggle &lt;id&gt;</code>");
    out
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn type_emoji(event_type: &str) -> &'static str {
    match event_type {
        "chat_in" => "💬",
        "chat_out" => "🤖",
        "tool_run" => "🔧",
        "task_start" => "🚀",
        "task_finish" => "✅",
        "task_fail" => "❌",
        _ => "•",
    }
}

/// Escape HTML special characters for safe embedding in Telegram HTML messages.
#[cfg(any(feature = "telegram", feature = "slack"))]
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ── LLM-powered conceptual explanation ──────────────────────────────

/// Generate a conceptual explanation of recent activity using LLM.
#[cfg(any(feature = "telegram", feature = "slack"))]
pub async fn generate_explanation(
    entries: &[ActivityEntry],
    llm: &dyn LlmProvider,
    detail: ReviewDetail,
) -> Result<String> {
    if entries.is_empty() {
        return Ok("No activity to explain.".to_string());
    }

    let prompt = build_explanation_prompt(entries, detail);

    let messages = vec![Message {
        role: ChatRole::User,
        content: prompt,
    }];

    let response = llm.chat(&messages).await?;
    Ok(response)
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn build_explanation_prompt(entries: &[ActivityEntry], detail: ReviewDetail) -> String {
    let activity_dump = build_activity_dump(entries);
    match detail {
        ReviewDetail::Summary => format!(
            "You are explaining what an AI agent system (Athena) did during a session.\n\
             Give a concise 2-3 sentence conceptual summary. Focus on WHAT was accomplished \
             and WHY, not implementation details. Write for a product manager.\n\n\
             Activity log:\n{}\n\nExplain:",
            activity_dump
        ),
        ReviewDetail::Standard => format!(
            "You are explaining what an AI agent system (Athena) did during a session.\n\
             Give a structured explanation with:\n\
             1. **What happened** — conceptual overview (2-3 sentences)\n\
             2. **Key decisions** — what reasoning drove the actions\n\
             3. **Tools & ghosts used** — why each was chosen\n\
             4. **Outcome** — what was achieved\n\n\
             Write at a logic/concept level, not code level. \
             Make it understandable for an engineer reviewing the session.\n\n\
             Activity log:\n{}\n\nExplain:",
            activity_dump
        ),
        ReviewDetail::Detailed => format!(
            "You are explaining what an AI agent system (Athena) did during a session.\n\
             Give a comprehensive explanation with:\n\
             1. **Executive summary** — 2-3 sentence overview\n\
             2. **Phase breakdown** — group events into logical phases, explain each\n\
             3. **Decision reasoning** — for each major decision, explain why\n\
             4. **Tool chain analysis** — what tools were used in what sequence and why\n\
             5. **Ghost strategy** — which agents were invoked and the reasoning\n\
             6. **What could be improved** — any inefficiencies or concerns\n\
             7. **How this fits the bigger picture** — connect to the overall system goals\n\n\
             Be thorough but conceptual. Write for a senior engineer who wants \
             full understanding without reading raw logs.\n\n\
             Activity log:\n{}\n\nExplain:",
            activity_dump
        ),
    }
}

#[cfg(any(feature = "telegram", feature = "slack"))]
fn build_activity_dump(entries: &[ActivityEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let mut line = format!("[{}] {} — {}", e.created_at, e.event_type, e.summary);
            if let Some(ref d) = e.detail {
                let trunc = truncate_str(d, 300);
                line.push_str(&format!(" | {}", trunc));
            }
            if let Some(ref g) = e.ghost {
                line.push_str(&format!(" [ghost:{}]", g));
            }
            if let Some(ref t) = e.tool_name {
                line.push_str(&format!(" [tool:{}]", t));
            }
            if let Some(ref input) = e.tool_input {
                line.push_str(&format!(" input:{}", truncate_str(input, 150)));
            }
            if let Some(ref output) = e.tool_output {
                line.push_str(&format!(" output:{}", truncate_str(output, 150)));
            }
            if e.parent_id.is_some() {
                line = format!("  (sub) {}", line);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(all(test, any(feature = "telegram", feature = "slack")))]
mod tests {
    use super::*;

    fn test_store() -> ActivityLogStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_activity_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key TEXT NOT NULL,
                event_type TEXT NOT NULL,
                summary TEXT NOT NULL,
                detail TEXT,
                ghost TEXT,
                tool_name TEXT,
                task_id TEXT,
                duration_ms INTEGER,
                tool_input TEXT,
                tool_output TEXT,
                parent_id INTEGER,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_session_activity_session_time
                ON session_activity_log(session_key, created_at DESC);
            CREATE TABLE review_alert_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                pattern TEXT NOT NULL,
                target TEXT NOT NULL DEFAULT 'tool_name',
                severity TEXT NOT NULL DEFAULT 'warn',
                enabled INTEGER NOT NULL DEFAULT 1,
                chat_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();
        ActivityLogStore::new(conn)
    }

    #[test]
    fn test_record_and_recent() {
        let store = test_store();
        store
            .record(
                "tg:123:456",
                ActivityEventType::ChatIn,
                "User asked about deployment",
                Some("How do I deploy to prod?"),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .record(
                "tg:123:456",
                ActivityEventType::ChatOut,
                "Explained deployment process",
                Some("To deploy to production, you need to..."),
                Some("mentor"),
                None,
                None,
                Some(1200),
            )
            .unwrap();
        store
            .record(
                "tg:123:456",
                ActivityEventType::ToolRun,
                "Executed shell command",
                Some("git status"),
                None,
                Some("shell"),
                None,
                Some(45),
            )
            .unwrap();

        let entries = store.recent("tg:123:456", 10).unwrap();
        assert_eq!(entries.len(), 3);
        // Check that all event types are present (order depends on timestamp resolution)
        let types: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"chat_in"));
        assert!(types.contains(&"chat_out"));
        assert!(types.contains(&"tool_run"));
        // Check tool_name on the tool_run entry
        let tool_entry = entries.iter().find(|e| e.event_type == "tool_run").unwrap();
        assert_eq!(tool_entry.tool_name.as_deref(), Some("shell"));
    }

    #[test]
    fn test_record_tool_with_details() {
        let store = test_store();
        let parent_id = store
            .record(
                "s1",
                ActivityEventType::AutonomousTaskStart,
                "Task started",
                None,
                Some("coder"),
                None,
                Some("task-1"),
                None,
            )
            .unwrap();

        let child_id = store
            .record_tool(
                "s1",
                "shell",
                "git status",
                Some("git status --porcelain"),
                Some("M src/main.rs\n?? new_file.txt"),
                Some("coder"),
                Some("task-1"),
                Some(120),
                Some(parent_id),
            )
            .unwrap();

        let entries = store.recent("s1", 10).unwrap();
        assert_eq!(entries.len(), 2);

        let tool_entry = entries.iter().find(|e| e.id == child_id).unwrap();
        assert_eq!(tool_entry.tool_input.as_deref(), Some("git status --porcelain"));
        assert_eq!(tool_entry.parent_id, Some(parent_id));

    }

    #[test]
    fn test_search() {
        let store = test_store();
        store
            .record(
                "s1",
                ActivityEventType::ChatIn,
                "Asked about deployment",
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .record(
                "s2",
                ActivityEventType::ToolRun,
                "Ran build command",
                None,
                None,
                Some("shell"),
                None,
                None,
            )
            .unwrap();
        store
            .record_tool(
                "s2",
                "shell",
                "git push",
                Some("git push origin main"),
                Some("Everything up-to-date"),
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let results = store.search("deployment", 10).unwrap();
        assert_eq!(results.len(), 1);

        let results = store.search("git push", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].tool_input.as_deref().unwrap().contains("git push"));
    }

    #[test]
    fn test_alert_rules() {
        let store = test_store();
        let id = store
            .add_alert_rule("sensitive files", ".env", "tool_input", "critical", None)
            .unwrap();

        let rules = store.list_alert_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "sensitive files");

        // Test alert matching
        let entry = ActivityEntry {
            id: 1,
            session_key: "s1".into(),
            event_type: "tool_run".into(),
            summary: "Read file".into(),
            detail: None,
            ghost: None,
            tool_name: Some("read_file".into()),
            task_id: None,
            duration_ms: None,
            tool_input: Some("cat .env".into()),
            tool_output: Some("SECRET_KEY=abc".into()),
            parent_id: None,
            created_at: "2025-01-01 10:00:00".into(),
        };

        let matches = store.check_alerts(&entry).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].rule.severity, "critical");

        // Non-matching entry
        let safe_entry = ActivityEntry {
            tool_input: Some("cat README.md".into()),
            ..entry
        };
        let matches = store.check_alerts(&safe_entry).unwrap();
        assert_eq!(matches.len(), 0);

        // Toggle and remove
        store.toggle_alert_rule(id, false).unwrap();
        let rules = store.list_alert_rules().unwrap();
        assert!(!rules[0].enabled);

        store.remove_alert_rule(id).unwrap();
        let rules = store.list_alert_rules().unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_render_summary() {
        let entries = vec![
            ActivityEntry {
                id: 1,
                session_key: "s1".into(),
                event_type: "chat_in".into(),
                summary: "Asked about X".into(),
                detail: None,
                ghost: None,
                tool_name: None,
                task_id: None,
                duration_ms: None,
                tool_input: None,
                tool_output: None,
                parent_id: None,
                created_at: "2025-01-01 10:00:00".into(),
            },
            ActivityEntry {
                id: 2,
                session_key: "s1".into(),
                event_type: "tool_run".into(),
                summary: "Ran shell cmd".into(),
                detail: None,
                ghost: None,
                tool_name: Some("shell".into()),
                task_id: None,
                duration_ms: Some(50),
                tool_input: Some("ls -la".into()),
                tool_output: Some("total 42\ndrwxr-xr-x ...".into()),
                parent_id: None,
                created_at: "2025-01-01 10:01:00".into(),
            },
            ActivityEntry {
                id: 3,
                session_key: "s1".into(),
                event_type: "chat_out".into(),
                summary: "Replied with answer".into(),
                detail: None,
                ghost: Some("mentor".into()),
                tool_name: None,
                task_id: None,
                duration_ms: Some(800),
                tool_input: None,
                tool_output: None,
                parent_id: None,
                created_at: "2025-01-01 10:02:00".into(),
            },
        ];

        let summary = render_review(&entries, ReviewDetail::Summary);
        assert!(summary.contains("Session Summary"));
        assert!(summary.contains("1 messages in"));
        assert!(summary.contains("1 tool executions"));

        let standard = render_review(&entries, ReviewDetail::Standard);
        assert!(standard.contains("Timeline"));
        assert!(standard.contains("shell"));
        assert!(standard.contains("ls -la")); // tool input preview

        let detailed = render_review(&entries, ReviewDetail::Detailed);
        assert!(detailed.contains("Input:"));
        assert!(detailed.contains("Output:"));
    }

    #[test]
    fn test_render_empty() {
        let result = render_review(&[], ReviewDetail::Summary);
        assert!(result.contains("No activity"));
    }

    #[test]
    fn test_render_search_results() {
        let entries = vec![ActivityEntry {
            id: 1,
            session_key: "s1".into(),
            event_type: "tool_run".into(),
            summary: "Ran deploy".into(),
            detail: None,
            ghost: None,
            tool_name: Some("shell".into()),
            task_id: None,
            duration_ms: None,
            tool_input: Some("deploy.sh".into()),
            tool_output: None,
            parent_id: None,
            created_at: "2025-01-01 10:00:00".into(),
        }];
        let result = render_search_results(&entries, "deploy");
        assert!(result.contains("1 matches"));
        assert!(result.contains("deploy.sh"));
    }
}
