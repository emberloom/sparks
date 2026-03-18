//! Proactive alerting engine.
//!
//! Runs in the background, periodically evaluating alert rules against the
//! activity log. Delivers alerts via the configured channel (log, webhook,
//! Slack, or Teams).

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::AlertsConfig;

#[cfg(feature = "telegram")]
use crate::session_review::ActivityLogStore;

/// A fired alert event.
#[derive(Debug, Clone)]
pub struct FiredAlert {
    pub rule_id: i64,
    pub rule_name: String,
    pub severity: String,
    pub pattern: String,
    pub matched_summary: String,
    pub target: String,
    pub fired_at: chrono::DateTime<chrono::Utc>,
}

/// Delivery channel for alert notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryChannel {
    Log,
    Webhook(String),
    Slack,
    Teams,
}

impl DeliveryChannel {
    /// Build a delivery channel from config strings.
    ///
    /// If `channel` is `"webhook"` but `webhook_url` is `None`, the resulting
    /// `Webhook("")` variant is intentionally invalid; `deliver()` will log a
    /// warning and skip delivery rather than panicking.
    pub fn from_config(channel: &str, webhook_url: Option<&str>) -> Self {
        match channel {
            "webhook" => DeliveryChannel::Webhook(webhook_url.unwrap_or("").to_string()),
            "slack" => DeliveryChannel::Slack,
            "teams" => DeliveryChannel::Teams,
            _ => DeliveryChannel::Log,
        }
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "critical" => 3,
        "warning" => 2,
        "info" => 1,
        _ => 0,
    }
}

/// The core alerting engine.
pub struct AlertEngine {
    config: AlertsConfig,
    #[cfg(feature = "telegram")]
    log: Arc<ActivityLogStore>,
    http: reqwest::Client,
    /// Last fired: rule_id -> Instant (for silence window).
    ///
    /// NOTE: `std::time::Instant` is not persisted across process restarts.
    /// After a restart the silence window resets and each alert rule may fire
    /// immediately on the first evaluation tick. This is intentional: the
    /// alternative (persisting wall-clock timestamps to disk) adds complexity
    /// that is not warranted for the current use case.
    last_fired: Arc<Mutex<HashMap<i64, std::time::Instant>>>,
}

impl AlertEngine {
    #[cfg(feature = "telegram")]
    pub fn new(config: AlertsConfig, log: Arc<ActivityLogStore>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("HTTP client");
        Self {
            config,
            log,
            http,
            last_fired: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(not(feature = "telegram"))]
    pub fn new(config: AlertsConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("HTTP client");
        Self {
            config,
            http,
            last_fired: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Evaluate all alert rules against recent activity log entries.
    #[cfg(feature = "telegram")]
    pub async fn evaluate(&self) -> Vec<FiredAlert> {
        let rules = match self.log.list_alert_rules() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "Failed to list alert rules");
                return vec![];
            }
        };

        let min_rank = severity_rank(&self.config.min_severity);
        let mut fired = Vec::new();
        let now = std::time::Instant::now();
        let silence = std::time::Duration::from_secs(self.config.silence_secs);

        let mut last_fired = self.last_fired.lock().await;

        for rule in &rules {
            if !rule.enabled {
                continue;
            }
            if severity_rank(&rule.severity) < min_rank {
                continue;
            }
            // Check silence window
            if let Some(last) = last_fired.get(&rule.id) {
                if now.duration_since(*last) < silence {
                    continue;
                }
            }

            // Search recent entries for pattern match
            let entries = self.log.search(&rule.pattern, 10).unwrap_or_default();
            if entries.is_empty() {
                continue;
            }

            // Check if any entry is within the last check_interval (new activity).
            // We use 1.5x the interval to tolerate minor scheduling jitter without
            // accumulating a window large enough to re-fire stale entries after a
            // silence period expires.
            let cutoff = chrono::Utc::now()
                - chrono::Duration::milliseconds(
                    (self.config.check_interval_secs as f64 * 1.5 * 1000.0) as i64,
                );
            let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
            let recent: Vec<_> = entries.iter().filter(|e| e.created_at >= cutoff_str).collect();
            if recent.is_empty() {
                continue;
            }

            let matched_summary = recent[0].summary.clone();
            last_fired.insert(rule.id, now);

            fired.push(FiredAlert {
                rule_id: rule.id,
                rule_name: rule.name.clone(),
                severity: rule.severity.clone(),
                pattern: rule.pattern.clone(),
                matched_summary,
                target: rule.target.clone(),
                fired_at: chrono::Utc::now(),
            });
        }
        fired
    }

    #[cfg(not(feature = "telegram"))]
    pub async fn evaluate(&self) -> Vec<FiredAlert> {
        // Alert rule storage requires the telegram feature (ActivityLogStore).
        vec![]
    }

    /// Deliver a fired alert to the configured channel.
    pub async fn deliver(&self, alert: &FiredAlert) {
        let channel = DeliveryChannel::from_config(
            &self.config.delivery_channel,
            self.config.webhook_url.as_deref(),
        );

        let emoji = match alert.severity.as_str() {
            "critical" => "\u{1F6A8}",
            "warning" => "\u{26A0}\u{FE0F}",
            _ => "\u{2139}\u{FE0F}",
        };
        let message = format!(
            "{} [{}] Alert: {} \u{2014} matched \"{}\" in activity log",
            emoji,
            alert.severity.to_uppercase(),
            alert.rule_name,
            alert.matched_summary
        );

        match &channel {
            DeliveryChannel::Log => {
                match alert.severity.as_str() {
                    "critical" => tracing::error!(
                        alert = %alert.rule_name,
                        pattern = %alert.pattern,
                        "{}",
                        message
                    ),
                    "warning" => tracing::warn!(
                        alert = %alert.rule_name,
                        pattern = %alert.pattern,
                        "{}",
                        message
                    ),
                    _ => tracing::info!(
                        alert = %alert.rule_name,
                        pattern = %alert.pattern,
                        "{}",
                        message
                    ),
                }
            }
            DeliveryChannel::Webhook(url) => {
                if url.is_empty() {
                    tracing::warn!("Webhook delivery configured but no webhook_url set");
                    return;
                }
                let payload = serde_json::json!({
                    "alert": alert.rule_name,
                    "severity": alert.severity,
                    "pattern": alert.pattern,
                    "matched": alert.matched_summary,
                    "fired_at": alert.fired_at.to_rfc3339(),
                    "message": message,
                });
                match self.http.post(url).json(&payload).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::info!(alert = %alert.rule_name, "Alert delivered via webhook");
                    }
                    Ok(resp) => {
                        tracing::error!(
                            alert = %alert.rule_name,
                            status = %resp.status(),
                            "Webhook delivery failed"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            alert = %alert.rule_name,
                            error = %e,
                            "Webhook delivery error"
                        );
                    }
                }
            }
            DeliveryChannel::Slack | DeliveryChannel::Teams => {
                // When running as a Slack/Teams bot, alerts are delivered via the
                // respective pulse/event bus. Log for now; the bot modules can subscribe
                // to fired alerts via an mpsc channel in a future iteration.
                tracing::info!(
                    channel = %self.config.delivery_channel,
                    alert = %alert.rule_name,
                    "{}",
                    message
                );
            }
        }
    }

    /// Run the alerting engine loop until cancelled.
    pub async fn run(self: Arc<Self>) {
        if !self.config.enabled {
            tracing::debug!("Alert engine disabled");
            return;
        }
        // Guard against a zero-second interval, which would panic in
        // `tokio::time::interval`. Clamp to a minimum of 1 second.
        let interval_secs = self.config.check_interval_secs.max(1);
        tracing::info!(
            interval_secs,
            "Alert engine started"
        );
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            interval_secs,
        ));
        loop {
            interval.tick().await;
            let fired = self.evaluate().await;
            for alert in &fired {
                self.deliver(alert).await;
            }
            if !fired.is_empty() {
                tracing::info!(
                    count = fired.len(),
                    "Alert evaluation: {} rule(s) fired",
                    fired.len()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_rank_ordering() {
        assert!(severity_rank("critical") > severity_rank("warning"));
        assert!(severity_rank("warning") > severity_rank("info"));
        assert!(severity_rank("info") > severity_rank("unknown"));
    }

    #[test]
    fn delivery_channel_from_config_log() {
        assert_eq!(DeliveryChannel::from_config("log", None), DeliveryChannel::Log);
    }

    #[test]
    fn delivery_channel_from_config_webhook() {
        let url = "https://hooks.example.com/abc";
        assert_eq!(
            DeliveryChannel::from_config("webhook", Some(url)),
            DeliveryChannel::Webhook(url.to_string()),
        );
    }

    #[test]
    fn delivery_channel_from_config_webhook_no_url() {
        // When channel is "webhook" but no URL is provided, from_config produces
        // Webhook(""). The deliver() method will log a warning and skip delivery
        // rather than panicking.
        assert_eq!(
            DeliveryChannel::from_config("webhook", None),
            DeliveryChannel::Webhook("".to_string()),
        );
    }

    #[test]
    fn delivery_channel_from_config_slack() {
        assert_eq!(DeliveryChannel::from_config("slack", None), DeliveryChannel::Slack);
    }

    #[test]
    fn delivery_channel_from_config_teams() {
        assert_eq!(DeliveryChannel::from_config("teams", None), DeliveryChannel::Teams);
    }

    #[test]
    fn fired_alert_message_format() {
        let alert = FiredAlert {
            rule_id: 1,
            rule_name: "test_alert".to_string(),
            severity: "critical".to_string(),
            pattern: "error".to_string(),
            matched_summary: "Task failed".to_string(),
            target: "log".to_string(),
            fired_at: chrono::Utc::now(),
        };
        let msg = format!(
            "\u{1F6A8} [{}] Alert: {} \u{2014} matched \"{}\" in activity log",
            alert.severity.to_uppercase(),
            alert.rule_name,
            alert.matched_summary
        );
        assert!(msg.contains("CRITICAL"));
        assert!(msg.contains("test_alert"));
        assert!(msg.contains("Task failed"));
    }
}
