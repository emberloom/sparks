//! Token usage and cost tracking.
//!
//! Records per-call token counts and calculates USD cost based on model pricing.
//! Enforces daily and per-session budgets when configured.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::config::CostConfig;
use crate::error::{SparksError, Result};

/// Built-in model pricing: (input_per_1m_usd, output_per_1m_usd).
/// Users can override via config.cost.model_prices.
pub fn builtin_prices() -> HashMap<&'static str, (f64, f64)> {
    let mut m = HashMap::new();
    // Anthropic
    m.insert("claude-opus-4-6",       (15.00, 75.00));
    m.insert("claude-sonnet-4-6",     (3.00,  15.00));
    m.insert("claude-haiku-4-5",      (0.80,  4.00));
    // OpenAI
    m.insert("gpt-4o",                (5.00,  15.00));
    m.insert("gpt-4o-mini",           (0.15,  0.60));
    m.insert("gpt-4-turbo",           (10.00, 30.00));
    m.insert("o1",                    (15.00, 60.00));
    m.insert("o3-mini",               (1.10,  4.40));
    // Common aliases
    m.insert("gpt-4",                 (30.00, 60.00));
    m.insert("gpt-3.5-turbo",         (0.50,  1.50));
    m
}

/// A single token usage record.
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub session_key: String,
    pub model: String,
    pub ghost: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Aggregate cost summary.
#[derive(Debug, Default, Clone)]
pub struct CostSummary {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub by_model: HashMap<String, f64>,
    pub by_ghost: HashMap<String, f64>,
    pub record_count: usize,
}

impl CostSummary {
    pub fn format_report(&self) -> String {
        let mut lines = vec![
            format!("**\u{1f4b0} Cost Summary**"),
            format!(""),
            format!("Total: **${:.4}**", self.total_cost_usd),
            format!("Input tokens: {}", self.total_input_tokens),
            format!("Output tokens: {}", self.total_output_tokens),
            format!("Calls: {}", self.record_count),
        ];

        if !self.by_model.is_empty() {
            lines.push(String::new());
            lines.push("**By model:**".to_string());
            let mut models: Vec<_> = self.by_model.iter().collect();
            models.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (model, cost) in models {
                lines.push(format!("  \u{2022} {}: ${:.4}", model, cost));
            }
        }

        if !self.by_ghost.is_empty() {
            lines.push(String::new());
            lines.push("**By ghost:**".to_string());
            let mut ghosts: Vec<_> = self.by_ghost.iter().collect();
            ghosts.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (ghost, cost) in ghosts {
                lines.push(format!("  \u{2022} {}: ${:.4}", ghost, cost));
            }
        }

        lines.join("\n")
    }
}

/// Calculate USD cost for given token counts and model.
///
/// Lookup precedence:
/// 1. Exact match in `config.model_prices` override map.
/// 2. Exact match in the built-in price table.
/// 3. Prefix match in the built-in table (e.g. "claude-sonnet-4-6-20251022" -> "claude-sonnet-4-6").
/// 4. Falls back to $0.00 for unknown models (zero-cost rather than an error).
///
/// Config override entries must have exactly two elements (`[input_per_1m, output_per_1m]`).
/// Missing elements default to 0.0 rather than panicking on an out-of-bounds index.
pub fn calculate_cost(model: &str, input_tokens: u64, output_tokens: u64, config: &CostConfig) -> f64 {
    // Check config overrides first.  Use .get() to avoid a bounds panic on malformed config.
    let prices = if let Some(override_prices) = config.model_prices.get(model) {
        let inp = override_prices.get(0).copied().unwrap_or(0.0);
        let out = override_prices.get(1).copied().unwrap_or(0.0);
        (inp, out)
    } else {
        let builtin = builtin_prices();
        // Try exact match, then prefix match
        if let Some(&(inp, out)) = builtin.get(model) {
            (inp, out)
        } else {
            // Try prefix: "claude-sonnet-4-6-20251022" -> "claude-sonnet-4-6"
            let matched = builtin.iter()
                .find(|(k, _)| model.starts_with(*k))
                .map(|(_, v)| *v);
            matched.unwrap_or((0.0, 0.0))
        }
    };
    (input_tokens as f64 / 1_000_000.0 * prices.0)
        + (output_tokens as f64 / 1_000_000.0 * prices.1)
}

/// Persistent cost tracker backed by SQLite.
pub struct CostTracker {
    conn: Mutex<Connection>,
    config: CostConfig,
}

impl CostTracker {
    pub fn new(conn: Connection, config: CostConfig) -> Result<Self> {
        {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cost_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_key TEXT NOT NULL,
                    model TEXT NOT NULL,
                    ghost TEXT,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cost_usd REAL NOT NULL DEFAULT 0.0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_cost_log_session ON cost_log(session_key);
                CREATE INDEX IF NOT EXISTS idx_cost_log_created ON cost_log(created_at);",
            )?;
        }
        Ok(Self { conn: Mutex::new(conn), config })
    }

    /// Record a token usage event.
    ///
    /// Does nothing when `config.cost.enabled` is false.
    pub fn record(&self, usage: &TokenUsage) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("cost lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO cost_log (session_key, model, ghost, input_tokens, output_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                usage.session_key,
                usage.model,
                usage.ghost,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cost_usd,
            ],
        )?;
        Ok(())
    }

    /// Get cost summary for today (UTC calendar day).
    pub fn today_summary(&self) -> Result<CostSummary> {
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("cost lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT model, ghost, input_tokens, output_tokens, cost_usd
             FROM cost_log WHERE date(created_at) >= date('now')"
        )?;
        Self::aggregate_rows_inner(&mut stmt, rusqlite::params![])
    }

    /// Get cost summary across all time.
    pub fn all_summary(&self) -> Result<CostSummary> {
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("cost lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT model, ghost, input_tokens, output_tokens, cost_usd FROM cost_log"
        )?;
        Self::aggregate_rows_inner(&mut stmt, rusqlite::params![])
    }

    /// Get cost summary for a specific session.
    pub fn session_summary(&self, session_key: &str) -> Result<CostSummary> {
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("cost lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT model, ghost, input_tokens, output_tokens, cost_usd
             FROM cost_log WHERE session_key = ?1"
        )?;
        Self::aggregate_rows_inner(&mut stmt, rusqlite::params![session_key])
    }

    fn aggregate_rows_inner(
        stmt: &mut rusqlite::Statement<'_>,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<CostSummary> {
        let mut summary = CostSummary::default();
        let rows = stmt.query_map(params, |row| {
            Ok((
                row.get::<_, String>(0)?,         // model
                row.get::<_, Option<String>>(1)?, // ghost
                row.get::<_, i64>(2)? as u64,     // input_tokens
                row.get::<_, i64>(3)? as u64,     // output_tokens
                row.get::<_, f64>(4)?,            // cost_usd
            ))
        })?;

        for row in rows {
            let (model, ghost, input, output, cost) = row?;
            summary.total_input_tokens += input;
            summary.total_output_tokens += output;
            summary.total_cost_usd += cost;
            summary.record_count += 1;
            *summary.by_model.entry(model).or_default() += cost;
            if let Some(g) = ghost {
                *summary.by_ghost.entry(g).or_default() += cost;
            }
        }
        Ok(summary)
    }

    /// Check whether the daily budget is exceeded.
    ///
    /// Returns `Ok(())` when tracking is disabled or no budget is configured.
    /// When `on_budget_exceeded = "block"` and the threshold is crossed, returns `Err`;
    /// otherwise emits a `tracing::warn`.
    pub fn check_daily_budget(&self) -> Result<()> {
        if self.config.daily_budget_usd <= 0.0 || !self.config.enabled {
            return Ok(());
        }
        let summary = self.today_summary()?;
        if summary.total_cost_usd >= self.config.daily_budget_usd {
            let msg = format!(
                "Daily cost budget exceeded: ${:.4} >= ${:.2}",
                summary.total_cost_usd, self.config.daily_budget_usd
            );
            if self.config.on_budget_exceeded == "block" {
                return Err(SparksError::Tool(msg));
            } else {
                tracing::warn!("{}", msg);
            }
        }
        Ok(())
    }

    /// Check whether the per-session budget is exceeded for the given session key.
    ///
    /// Returns `Ok(())` when tracking is disabled or no session budget is configured.
    /// When `on_budget_exceeded = "block"` and the threshold is crossed, returns `Err`;
    /// otherwise emits a `tracing::warn`.
    pub fn check_session_budget(&self, session_key: &str) -> Result<()> {
        if self.config.session_budget_usd <= 0.0 || !self.config.enabled {
            return Ok(());
        }
        let summary = self.session_summary(session_key)?;
        if summary.total_cost_usd >= self.config.session_budget_usd {
            let msg = format!(
                "Session cost budget exceeded for '{}': ${:.4} >= ${:.2}",
                session_key, summary.total_cost_usd, self.config.session_budget_usd
            );
            if self.config.on_budget_exceeded == "block" {
                return Err(SparksError::Tool(msg));
            } else {
                tracing::warn!("{}", msg);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculate_cost_known_model() {
        let config = CostConfig::default();
        // claude-sonnet: $3/1M input, $15/1M output
        let cost = calculate_cost("claude-sonnet-4-6", 1_000_000, 1_000_000, &config);
        assert!((cost - 18.0).abs() < 0.01, "Expected ~$18, got ${}", cost);
    }

    #[test]
    fn calculate_cost_zero_tokens() {
        let config = CostConfig::default();
        let cost = calculate_cost("claude-sonnet-4-6", 0, 0, &config);
        assert_eq!(cost, 0.0, "Zero tokens must produce zero cost");
    }

    #[test]
    fn calculate_cost_unknown_model() {
        let config = CostConfig::default();
        let cost = calculate_cost("unknown-model-xyz", 1000, 1000, &config);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn calculate_cost_config_override() {
        let mut config = CostConfig::default();
        config.model_prices.insert("my-model".to_string(), [10.0, 20.0]);
        let cost = calculate_cost("my-model", 1_000_000, 500_000, &config);
        assert!((cost - 20.0).abs() < 0.01); // $10 input + $10 output
    }

    #[test]
    fn calculate_cost_prefix_match() {
        let config = CostConfig::default();
        // Should match "claude-sonnet-4-6" prefix
        let cost = calculate_cost("claude-sonnet-4-6-20251022", 1_000_000, 0, &config);
        assert!((cost - 3.0).abs() < 0.01);
    }

    #[test]
    fn cost_tracker_record_and_summarize() {
        let conn = Connection::open_in_memory().unwrap();
        let tracker = CostTracker::new(conn, CostConfig::default()).unwrap();
        let usage = TokenUsage {
            session_key: "test:session".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            ghost: Some("coder".to_string()),
            input_tokens: 1000,
            output_tokens: 500,
            cost_usd: 0.0105,
        };
        tracker.record(&usage).unwrap();
        let summary = tracker.session_summary("test:session").unwrap();
        assert_eq!(summary.record_count, 1);
        assert_eq!(summary.total_input_tokens, 1000);
        assert!((summary.total_cost_usd - 0.0105).abs() < 0.0001);
        assert!(summary.by_ghost.contains_key("coder"));
    }

    #[test]
    fn cost_tracker_disabled_skips_record() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = CostConfig::default();
        cfg.enabled = false;
        let tracker = CostTracker::new(conn, cfg).unwrap();
        let usage = TokenUsage {
            session_key: "s".to_string(),
            model: "gpt-4o".to_string(),
            ghost: None,
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: 0.001,
        };
        tracker.record(&usage).unwrap();
        // Disabled tracker: no rows inserted, all_summary returns empty.
        let summary = tracker.all_summary().unwrap();
        assert_eq!(summary.record_count, 0);
    }

    #[test]
    fn check_daily_budget_warn_does_not_err() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = CostConfig::default();
        cfg.daily_budget_usd = 0.001; // very low limit
        cfg.on_budget_exceeded = "warn".to_string();
        let tracker = CostTracker::new(conn, cfg).unwrap();
        let usage = TokenUsage {
            session_key: "s".to_string(),
            model: "gpt-4o".to_string(),
            ghost: None,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cost_usd: 20.0, // well over limit
        };
        tracker.record(&usage).unwrap();
        // "warn" mode: check_daily_budget must return Ok (not Err)
        assert!(tracker.check_daily_budget().is_ok());
    }

    #[test]
    fn check_daily_budget_block_returns_err() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = CostConfig::default();
        cfg.daily_budget_usd = 0.001;
        cfg.on_budget_exceeded = "block".to_string();
        let tracker = CostTracker::new(conn, cfg).unwrap();
        let usage = TokenUsage {
            session_key: "s".to_string(),
            model: "gpt-4o".to_string(),
            ghost: None,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cost_usd: 20.0,
        };
        tracker.record(&usage).unwrap();
        assert!(tracker.check_daily_budget().is_err());
    }

    #[test]
    fn check_session_budget_block_returns_err() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = CostConfig::default();
        cfg.session_budget_usd = 0.001;
        cfg.on_budget_exceeded = "block".to_string();
        let tracker = CostTracker::new(conn, cfg).unwrap();
        let usage = TokenUsage {
            session_key: "test-session".to_string(),
            model: "gpt-4o".to_string(),
            ghost: None,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cost_usd: 5.0,
        };
        tracker.record(&usage).unwrap();
        assert!(tracker.check_session_budget("test-session").is_err());
        // A different session must not be affected.
        assert!(tracker.check_session_budget("other-session").is_ok());
    }

    #[test]
    fn cost_summary_format_report() {
        let mut summary = CostSummary::default();
        summary.total_cost_usd = 1.2345;
        summary.total_input_tokens = 100_000;
        summary.total_output_tokens = 50_000;
        summary.record_count = 10;
        summary.by_model.insert("claude-sonnet-4-6".to_string(), 1.2345);
        let report = summary.format_report();
        assert!(report.contains("$1.2345"));
        assert!(report.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn builtin_prices_non_empty() {
        let prices = builtin_prices();
        assert!(!prices.is_empty());
        assert!(prices.contains_key("claude-sonnet-4-6"));
        assert!(prices.contains_key("gpt-4o"));
    }
}
