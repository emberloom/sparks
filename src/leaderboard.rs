//! Agent performance leaderboard and A/B ghost comparison.
//!
//! Tracks per-ghost performance metrics (success rate, latency, token usage)
//! and supports A/B testing between ghost profiles.

use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::config::LeaderboardConfig;
use crate::error::{SparksError, Result};

/// A single task outcome record.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub session_key: String,
    pub ghost: String,
    pub success: bool,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub user_rating: Option<i8>,  // -1 (thumbs down), 0 (neutral), 1 (thumbs up)
}

/// Aggregated performance metrics for a ghost.
#[derive(Debug, Clone, Default)]
pub struct GhostMetrics {
    pub ghost: String,
    pub total_tasks: u64,
    pub successful_tasks: u64,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
    pub avg_input_tokens: f64,
    pub avg_output_tokens: f64,
    pub avg_rating: f64,
    pub rated_tasks: u64,
}

impl GhostMetrics {
    pub fn rank_score(&self) -> f64 {
        // Weighted composite: 60% success rate, 20% rating, 20% efficiency (inverse tokens)
        let efficiency = if self.avg_input_tokens + self.avg_output_tokens > 0.0 {
            1.0 - ((self.avg_input_tokens + self.avg_output_tokens) / 10_000.0).min(1.0)
        } else {
            0.5
        };
        let rating = if self.rated_tasks > 0 { (self.avg_rating + 1.0) / 2.0 } else { 0.5 };
        0.6 * self.success_rate + 0.2 * rating + 0.2 * efficiency
    }

    pub fn format_row(&self, rank: usize) -> String {
        let stars = match (self.success_rate * 10.0) as u32 {
            9..=10 => "★★★★★",
            7..=8  => "★★★★☆",
            5..=6  => "★★★☆☆",
            3..=4  => "★★☆☆☆",
            _      => "★☆☆☆☆",
        };
        format!(
            "#{} {:20} {} {:.0}% success  {:.0}ms avg  {} tasks",
            rank, self.ghost, stars,
            self.success_rate * 100.0,
            self.avg_latency_ms,
            self.total_tasks,
        )
    }
}

/// A/B test routing result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbRoute {
    Control,
    Challenger(String),
}

/// The leaderboard store backed by SQLite.
pub struct GhostLeaderboard {
    conn: Mutex<Connection>,
    config: LeaderboardConfig,
}

impl GhostLeaderboard {
    pub fn new(conn: Connection, config: LeaderboardConfig) -> Result<Self> {
        {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS ghost_outcomes (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_key TEXT NOT NULL,
                    ghost TEXT NOT NULL,
                    success INTEGER NOT NULL DEFAULT 0,
                    latency_ms INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    user_rating INTEGER,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_ghost_outcomes_ghost ON ghost_outcomes(ghost);
                CREATE INDEX IF NOT EXISTS idx_ghost_outcomes_created ON ghost_outcomes(created_at);",
            )?;
        }
        Ok(Self { conn: Mutex::new(conn), config })
    }

    /// Record a task outcome.
    pub fn record(&self, outcome: &TaskOutcome) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("lb lock".into()))?;
        conn.execute(
            "INSERT INTO ghost_outcomes (session_key, ghost, success, latency_ms, input_tokens, output_tokens, user_rating)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                outcome.session_key,
                outcome.ghost,
                outcome.success as i32,
                outcome.latency_ms as i64,
                outcome.input_tokens as i64,
                outcome.output_tokens as i64,
                outcome.user_rating.map(|r| r as i64),
            ],
        )?;
        Ok(())
    }

    /// Get metrics for all ghosts, sorted by rank score.
    pub fn rankings(&self) -> Result<Vec<GhostMetrics>> {
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("lb lock".into()))?;
        let mut stmt = conn.prepare(
            "SELECT ghost,
                    COUNT(*) as total,
                    SUM(success) as successes,
                    AVG(latency_ms) as avg_latency,
                    AVG(input_tokens) as avg_input,
                    AVG(output_tokens) as avg_output,
                    AVG(CASE WHEN user_rating IS NOT NULL THEN CAST(user_rating AS REAL) END) as avg_rating,
                    COUNT(user_rating) as rated
             FROM ghost_outcomes
             GROUP BY ghost
             ORDER BY total DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, f64>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, Option<f64>>(6)?,
                row.get::<_, i64>(7)? as u64,
            ))
        })?;

        let mut metrics = Vec::new();
        for row in rows {
            let (ghost, total, successes, avg_latency, avg_input, avg_output, avg_rating, rated) = row?;
            let success_rate = if total > 0 { successes as f64 / total as f64 } else { 0.0 };
            metrics.push(GhostMetrics {
                ghost,
                total_tasks: total,
                successful_tasks: successes,
                success_rate,
                avg_latency_ms: avg_latency,
                avg_input_tokens: avg_input,
                avg_output_tokens: avg_output,
                avg_rating: avg_rating.unwrap_or(0.0),
                rated_tasks: rated,
            });
        }

        metrics.sort_by(|a, b| b.rank_score().partial_cmp(&a.rank_score()).unwrap_or(std::cmp::Ordering::Equal));
        Ok(metrics)
    }

    /// Get metrics for a specific ghost.
    pub fn ghost_metrics(&self, ghost: &str) -> Result<Option<GhostMetrics>> {
        Ok(self.rankings()?.into_iter().find(|m| m.ghost == ghost))
    }

    /// Determine A/B route for an incoming request.
    pub fn ab_route(&self) -> AbRoute {
        let challenger = match &self.config.ab_test_ghost {
            Some(g) if !g.is_empty() => g.clone(),
            _ => return AbRoute::Control,
        };
        if rand::random::<f64>() < self.config.ab_test_fraction {
            AbRoute::Challenger(challenger)
        } else {
            AbRoute::Control
        }
    }

    /// Check if the challenger ghost should be promoted based on performance.
    pub fn check_promotion(&self) -> Result<Option<String>> {
        let challenger = match &self.config.ab_test_ghost {
            Some(g) if !g.is_empty() => g.clone(),
            _ => return Ok(None),
        };

        let rankings = self.rankings()?;
        let challenger_metrics = rankings.iter().find(|m| m.ghost == challenger);
        let Some(challenger_m) = challenger_metrics else { return Ok(None); };

        if challenger_m.total_tasks < self.config.min_samples_for_recommendation {
            return Ok(None);
        }

        // Find the best non-challenger ghost (control)
        let control_metrics = rankings.iter().find(|m| m.ghost != challenger);
        let Some(control_m) = control_metrics else { return Ok(None); };

        let improvement = challenger_m.success_rate - control_m.success_rate;
        if improvement >= self.config.promotion_threshold {
            Ok(Some(format!(
                "Recommendation: promote ghost '{}' -- {:.1}% success rate vs {:.1}% for '{}' (+{:.1}% over {}+ tasks)",
                challenger,
                challenger_m.success_rate * 100.0,
                control_m.success_rate * 100.0,
                control_m.ghost,
                improvement * 100.0,
                challenger_m.total_tasks,
            )))
        } else {
            Ok(None)
        }
    }

    /// Format the leaderboard as an ASCII table.
    pub fn format_leaderboard(&self) -> Result<String> {
        let rankings = self.rankings()?;
        if rankings.is_empty() {
            return Ok("No performance data recorded yet. Complete some tasks to see the leaderboard.".to_string());
        }

        let mut lines = vec![
            "Ghost Leaderboard".to_string(),
            "-".repeat(60),
        ];
        for (i, metrics) in rankings.iter().enumerate() {
            lines.push(metrics.format_row(i + 1));
        }
        lines.push("-".repeat(60));

        if let Ok(Some(promo)) = self.check_promotion() {
            lines.push(String::new());
            lines.push(promo);
        }

        Ok(lines.join("\n"))
    }

    /// Compare two ghosts head-to-head.
    pub fn compare(&self, ghost_a: &str, ghost_b: &str) -> Result<String> {
        let m_a = self.ghost_metrics(ghost_a)?.ok_or_else(|| SparksError::Tool(format!("No data for ghost '{}'", ghost_a)))?;
        let m_b = self.ghost_metrics(ghost_b)?.ok_or_else(|| SparksError::Tool(format!("No data for ghost '{}'", ghost_b)))?;

        let winner_success = if m_a.success_rate >= m_b.success_rate { ghost_a } else { ghost_b };
        let winner_speed = if m_a.avg_latency_ms <= m_b.avg_latency_ms { ghost_a } else { ghost_b };

        Ok(format!(
            "Ghost Comparison: {} vs {}\n\n\
             {:>30}  {:<30}\n\
             {:>30}  {:<30}\n\
             {:>30}  {:<30}\n\
             {:>30}  {:<30}\n\
             {:>30}  {:<30}\n\n\
             Success rate winner: {}\n\
             Speed winner: {}",
            ghost_a, ghost_b,
            format!("Success: {:.1}%", m_a.success_rate * 100.0),
            format!("Success: {:.1}%", m_b.success_rate * 100.0),
            format!("Avg latency: {:.0}ms", m_a.avg_latency_ms),
            format!("Avg latency: {:.0}ms", m_b.avg_latency_ms),
            format!("Tasks: {}", m_a.total_tasks),
            format!("Tasks: {}", m_b.total_tasks),
            format!("Avg input: {:.0} tok", m_a.avg_input_tokens),
            format!("Avg input: {:.0} tok", m_b.avg_input_tokens),
            format!("Score: {:.3}", m_a.rank_score()),
            format!("Score: {:.3}", m_b.rank_score()),
            winner_success, winner_speed,
        ))
    }

    /// Reset all leaderboard data.
    pub fn reset(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| SparksError::Internal("lb lock".into()))?;
        conn.execute("DELETE FROM ghost_outcomes", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lb() -> GhostLeaderboard {
        let conn = Connection::open_in_memory().unwrap();
        GhostLeaderboard::new(conn, LeaderboardConfig::default()).unwrap()
    }

    #[test]
    fn record_and_rank() {
        let lb = test_lb();
        for i in 0..10 {
            lb.record(&TaskOutcome {
                session_key: "s".into(),
                ghost: "coder".into(),
                success: i % 3 != 0,  // ~67% success
                latency_ms: 1000,
                input_tokens: 500,
                output_tokens: 200,
                user_rating: None,
            }).unwrap();
        }
        let rankings = lb.rankings().unwrap();
        assert_eq!(rankings.len(), 1);
        assert_eq!(rankings[0].ghost, "coder");
        assert_eq!(rankings[0].total_tasks, 10);
    }

    #[test]
    fn ab_route_control_when_no_challenger() {
        let lb = test_lb();
        assert_eq!(lb.ab_route(), AbRoute::Control);
    }

    #[test]
    fn ab_route_always_control_when_fraction_zero() {
        let mut config = LeaderboardConfig::default();
        config.ab_test_ghost = Some("challenger".into());
        config.ab_test_fraction = 0.0;
        let conn = Connection::open_in_memory().unwrap();
        let lb = GhostLeaderboard::new(conn, config).unwrap();
        // With fraction 0, should always be Control
        for _ in 0..20 {
            assert_eq!(lb.ab_route(), AbRoute::Control);
        }
    }

    #[test]
    fn metrics_rank_score_success_dominant() {
        let high = GhostMetrics { ghost: "h".into(), success_rate: 1.0, total_tasks: 10, ..Default::default() };
        let low  = GhostMetrics { ghost: "l".into(), success_rate: 0.0, total_tasks: 10, ..Default::default() };
        assert!(high.rank_score() > low.rank_score());
    }

    #[test]
    fn format_leaderboard_empty() {
        let lb = test_lb();
        let text = lb.format_leaderboard().unwrap();
        assert!(text.contains("No performance data"));
    }

    #[test]
    fn compare_two_ghosts() {
        let lb = test_lb();
        lb.record(&TaskOutcome { session_key: "s".into(), ghost: "alpha".into(), success: true, latency_ms: 800, input_tokens: 300, output_tokens: 100, user_rating: None }).unwrap();
        lb.record(&TaskOutcome { session_key: "s".into(), ghost: "beta".into(), success: false, latency_ms: 1200, input_tokens: 600, output_tokens: 200, user_rating: None }).unwrap();
        let result = lb.compare("alpha", "beta").unwrap();
        assert!(result.contains("alpha"));
        assert!(result.contains("beta"));
    }

    #[test]
    fn promotion_check_insufficient_samples() {
        let mut config = LeaderboardConfig::default();
        config.ab_test_ghost = Some("challenger".into());
        config.min_samples_for_recommendation = 50;
        let conn = Connection::open_in_memory().unwrap();
        let lb = GhostLeaderboard::new(conn, config).unwrap();
        lb.record(&TaskOutcome { session_key: "s".into(), ghost: "challenger".into(), success: true, latency_ms: 500, input_tokens: 200, output_tokens: 80, user_rating: None }).unwrap();
        // Only 1 sample, need 50
        assert!(lb.check_promotion().unwrap().is_none());
    }
}
