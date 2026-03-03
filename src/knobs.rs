use std::fmt::Write;
use std::sync::{Arc, RwLock};

use crate::config::Config;

/// Runtime-tunable knobs for all proactive behavior.
/// Shared as `Arc<RwLock<RuntimeKnobs>>` across all background tasks.
#[derive(Debug, Clone)]
pub struct RuntimeKnobs {
    // Master switch
    pub all_proactive: bool,

    // Per-feature toggles
    pub heartbeat_enabled: bool,
    pub cron_enabled: bool,
    pub mood_enabled: bool,
    pub memory_scan_enabled: bool,
    pub idle_musings_enabled: bool,
    pub conversation_reentry_enabled: bool,
    pub ticket_intake_enabled: bool,

    pub reentry_delay_secs: u64,
    pub reentry_jitter: f64,
    pub relationship_tracking_enabled: bool,
    pub mood_injection_enabled: bool,

    // Tunable parameters
    pub spontaneity: f32,
    pub heartbeat_interval_secs: u64,
    pub heartbeat_jitter: f64,
    pub memory_scan_interval_secs: u64,
    pub idle_threshold_secs: u64,
    pub ticket_intake_interval_secs: u64,
    pub mood_drift_interval_secs: u64,
    pub pulse_tolerance: f32,
    pub quiet_hours: Option<(u32, u32)>,
    pub timezone_offset: i32,
    pub cli_tool: String,
    pub cli_model: String,

    // Self-development
    pub self_dev_enabled: bool,
    pub metrics_interval_secs: u64,

    // Code indexer (Phase 3)
    pub code_indexer_enabled: bool,
    pub code_indexer_interval_secs: u64,
    pub refactoring_scan_enabled: bool,
    pub refactoring_scan_interval_secs: u64,

    // Langfuse observability
    pub langfuse_enabled: bool,
}

impl RuntimeKnobs {
    /// Initialize from config.toml values.
    pub fn from_config(config: &Config) -> Self {
        let quiet_hours = match (
            config.initiative.quiet_hours_start,
            config.initiative.quiet_hours_end,
        ) {
            (Some(s), Some(e)) => Some((s, e)),
            _ => None,
        };

        Self {
            all_proactive: config.proactive.enabled,
            heartbeat_enabled: config.heartbeat.enabled,
            cron_enabled: config.proactive.enabled,
            mood_enabled: config.mood.enabled,
            memory_scan_enabled: config.proactive.enabled,
            idle_musings_enabled: config.proactive.enabled,
            conversation_reentry_enabled: config.proactive.enabled,
            ticket_intake_enabled: config.ticket_intake.enabled,
            reentry_delay_secs: config.proactive.reentry_delay_secs,
            reentry_jitter: config.proactive.reentry_jitter,
            relationship_tracking_enabled: config.proactive.enabled,
            mood_injection_enabled: config.mood.enabled,
            spontaneity: config.proactive.spontaneity,
            heartbeat_interval_secs: config.heartbeat.interval_secs,
            heartbeat_jitter: config.heartbeat.jitter,
            memory_scan_interval_secs: config.proactive.memory_scan_interval_secs,
            idle_threshold_secs: config.proactive.idle_threshold_secs,
            ticket_intake_interval_secs: config.ticket_intake.poll_interval_secs,
            mood_drift_interval_secs: config.mood.drift_interval_secs,
            pulse_tolerance: config.initiative.tolerance,
            quiet_hours,
            timezone_offset: config.mood.timezone_offset,
            cli_tool: if std::env::var_os("CLAUDECODE").is_some() {
                "codex".to_string()
            } else {
                "claude_code".to_string()
            },
            cli_model: String::new(),
            self_dev_enabled: config.self_dev.enabled,
            metrics_interval_secs: config.self_dev.metrics_interval_secs,
            code_indexer_enabled: config.self_dev.code_indexer_enabled,
            code_indexer_interval_secs: config.self_dev.code_indexer_interval_secs,
            refactoring_scan_enabled: config.self_dev.refactoring_scan_enabled,
            refactoring_scan_interval_secs: config.self_dev.refactoring_scan_interval_secs,
            langfuse_enabled: config.langfuse.enabled
                || std::env::var("LANGFUSE_PUBLIC_KEY").is_ok(),
        }
    }

    /// Create a minimal instance for tests (no Config needed).
    #[cfg(test)]
    pub fn default_for_test() -> Self {
        Self {
            all_proactive: false,
            heartbeat_enabled: false,
            cron_enabled: false,
            mood_enabled: false,
            memory_scan_enabled: false,
            idle_musings_enabled: false,
            conversation_reentry_enabled: false,
            ticket_intake_enabled: false,
            reentry_delay_secs: 300,
            reentry_jitter: 0.2,
            relationship_tracking_enabled: false,
            mood_injection_enabled: false,
            spontaneity: 0.5,
            heartbeat_interval_secs: 600,
            heartbeat_jitter: 0.2,
            memory_scan_interval_secs: 300,
            idle_threshold_secs: 300,
            ticket_intake_interval_secs: 300,
            mood_drift_interval_secs: 600,
            pulse_tolerance: 0.5,
            quiet_hours: None,
            timezone_offset: 0,
            cli_tool: "claude_code".to_string(),
            cli_model: String::new(),
            self_dev_enabled: false,
            metrics_interval_secs: 30,
            code_indexer_enabled: false,
            code_indexer_interval_secs: 14400,
            refactoring_scan_enabled: false,
            refactoring_scan_interval_secs: 21600,
            langfuse_enabled: false,
        }
    }

    /// Parse a `/set key value` command. Returns a confirmation message.
    pub fn set(&mut self, key: &str, value: &str) -> Result<String, String> {
        match key {
            "all" => {
                let on = parse_bool(value)?;
                self.all_proactive = on;
                self.heartbeat_enabled = on;
                self.cron_enabled = on;
                self.mood_enabled = on;
                self.memory_scan_enabled = on;
                self.idle_musings_enabled = on;
                self.conversation_reentry_enabled = on;
                self.relationship_tracking_enabled = on;
                self.mood_injection_enabled = on;
                Ok(format!(
                    "All proactive features: {}",
                    if on { "on" } else { "off" }
                ))
            }
            "heartbeat" => {
                self.heartbeat_enabled = parse_bool(value)?;
                Ok(format!("Heartbeat: {}", on_off(self.heartbeat_enabled)))
            }
            "cron" => {
                self.cron_enabled = parse_bool(value)?;
                Ok(format!("Cron: {}", on_off(self.cron_enabled)))
            }
            "mood" => {
                let on = parse_bool(value)?;
                self.mood_enabled = on;
                self.mood_injection_enabled = on;
                Ok(format!("Mood: {}", on_off(on)))
            }
            "memory_scan" => {
                self.memory_scan_enabled = parse_bool(value)?;
                Ok(format!("Memory scan: {}", on_off(self.memory_scan_enabled)))
            }
            "idle_musings" => {
                self.idle_musings_enabled = parse_bool(value)?;
                Ok(format!(
                    "Idle musings: {}",
                    on_off(self.idle_musings_enabled)
                ))
            }
            "conversation_reentry" => {
                self.conversation_reentry_enabled = parse_bool(value)?;
                Ok(format!(
                    "Conversation re-entry: {}",
                    on_off(self.conversation_reentry_enabled)
                ))
            }
            "ticket_intake" => {
                self.ticket_intake_enabled = parse_bool(value)?;
                Ok(format!(
                    "Ticket intake: {}",
                    on_off(self.ticket_intake_enabled)
                ))
            }
            "reentry.delay" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.reentry_delay_secs = v.max(60);
                Ok(format!("Re-entry delay: {}s", self.reentry_delay_secs))
            }
            "reentry.jitter" => {
                let v: f64 = value
                    .parse()
                    .map_err(|_| "Expected float 0.0-1.0".to_string())?;
                self.reentry_jitter = v.clamp(0.0, 1.0);
                Ok(format!("Re-entry jitter: {:.2}", self.reentry_jitter))
            }
            "relationship_tracking" => {
                self.relationship_tracking_enabled = parse_bool(value)?;
                Ok(format!(
                    "Relationship tracking: {}",
                    on_off(self.relationship_tracking_enabled)
                ))
            }
            "mood_injection" => {
                self.mood_injection_enabled = parse_bool(value)?;
                Ok(format!(
                    "Mood injection: {}",
                    on_off(self.mood_injection_enabled)
                ))
            }
            "spontaneity" => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| "Expected float 0.0-1.0".to_string())?;
                self.spontaneity = v.clamp(0.0, 1.0);
                Ok(format!("Spontaneity: {:.2}", self.spontaneity))
            }
            "heartbeat.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.heartbeat_interval_secs = v.max(10);
                Ok(format!(
                    "Heartbeat interval: {}s",
                    self.heartbeat_interval_secs
                ))
            }
            "heartbeat.jitter" => {
                let v: f64 = value
                    .parse()
                    .map_err(|_| "Expected float 0.0-1.0".to_string())?;
                self.heartbeat_jitter = v.clamp(0.0, 1.0);
                Ok(format!("Heartbeat jitter: {:.2}", self.heartbeat_jitter))
            }
            "memory_scan.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.memory_scan_interval_secs = v.max(60);
                Ok(format!(
                    "Memory scan interval: {}s",
                    self.memory_scan_interval_secs
                ))
            }
            "idle_threshold" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.idle_threshold_secs = v.max(60);
                Ok(format!("Idle threshold: {}s", self.idle_threshold_secs))
            }
            "ticket_intake.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.ticket_intake_interval_secs = v.max(30);
                Ok(format!(
                    "Ticket intake interval: {}s",
                    self.ticket_intake_interval_secs
                ))
            }
            "mood.drift_interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.mood_drift_interval_secs = v.max(60);
                Ok(format!(
                    "Mood drift interval: {}s",
                    self.mood_drift_interval_secs
                ))
            }
            "tolerance" => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| "Expected float 0.0-1.0".to_string())?;
                self.pulse_tolerance = v.clamp(0.0, 1.0);
                Ok(format!("Pulse tolerance: {:.2}", self.pulse_tolerance))
            }
            "quiet_hours" => {
                if value == "off" || value == "none" {
                    self.quiet_hours = None;
                    Ok("Quiet hours: disabled".to_string())
                } else {
                    let parts: Vec<&str> = value.split('-').collect();
                    if parts.len() != 2 {
                        return Err("Expected format: start-end (e.g., 22-8)".to_string());
                    }
                    let start: u32 = parts[0]
                        .parse()
                        .map_err(|_| "Invalid start hour".to_string())?;
                    let end: u32 = parts[1]
                        .parse()
                        .map_err(|_| "Invalid end hour".to_string())?;
                    if start > 23 || end > 23 {
                        return Err("Hours must be 0-23".to_string());
                    }
                    self.quiet_hours = Some((start, end));
                    Ok(format!("Quiet hours: {}-{}", start, end))
                }
            }
            "timezone_offset" => {
                let v: i32 = value
                    .parse()
                    .map_err(|_| "Expected integer hours".to_string())?;
                self.timezone_offset = v.clamp(-12, 14);
                Ok(format!("Timezone offset: {}h", self.timezone_offset))
            }
            "cli_tool" => {
                const VALID: &[&str] = &["claude_code", "codex", "opencode"];
                if VALID.contains(&value) {
                    self.cli_tool = value.to_string();
                    Ok(format!("CLI tool: {}", self.cli_tool))
                } else {
                    Err(format!(
                        "Invalid CLI tool: {}. Valid: {}",
                        value,
                        VALID.join(", ")
                    ))
                }
            }
            "cli_model" => {
                if value == "reset" || value == "default" || value.is_empty() {
                    self.cli_model = String::new();
                    Ok("CLI model: default (tool decides)".to_string())
                } else {
                    self.cli_model = value.to_string();
                    Ok(format!("CLI model: {}", self.cli_model))
                }
            }
            "self_dev" => {
                self.self_dev_enabled = parse_bool(value)?;
                Ok(format!("Self-dev: {}", on_off(self.self_dev_enabled)))
            }
            "metrics.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.metrics_interval_secs = v.max(10);
                Ok(format!("Metrics interval: {}s", self.metrics_interval_secs))
            }
            "code_indexer" => {
                self.code_indexer_enabled = parse_bool(value)?;
                Ok(format!(
                    "Code indexer: {}",
                    on_off(self.code_indexer_enabled)
                ))
            }
            "code_indexer.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.code_indexer_interval_secs = v.max(600);
                Ok(format!(
                    "Code indexer interval: {}s",
                    self.code_indexer_interval_secs
                ))
            }
            "refactoring_scan" => {
                self.refactoring_scan_enabled = parse_bool(value)?;
                Ok(format!(
                    "Refactoring scan: {}",
                    on_off(self.refactoring_scan_enabled)
                ))
            }
            "refactoring_scan.interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "Expected integer seconds".to_string())?;
                self.refactoring_scan_interval_secs = v.max(600);
                Ok(format!(
                    "Refactoring scan interval: {}s",
                    self.refactoring_scan_interval_secs
                ))
            }
            "langfuse" => {
                let on = parse_bool(value)?;
                self.langfuse_enabled = on;
                Ok(format!("Langfuse: {}", on_off(self.langfuse_enabled)))
            }
            _ => Err(format!("Unknown knob: {}", key)),
        }
    }

    /// Pretty-print all current knob values.
    pub fn display(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Runtime Knobs:");
        let _ = writeln!(s, "  all_proactive          {}", on_off(self.all_proactive));
        let _ = writeln!(
            s,
            "  heartbeat              {}",
            on_off(self.heartbeat_enabled)
        );
        let _ = writeln!(s, "  cron                   {}", on_off(self.cron_enabled));
        let _ = writeln!(s, "  mood                   {}", on_off(self.mood_enabled));
        let _ = writeln!(
            s,
            "  memory_scan            {}",
            on_off(self.memory_scan_enabled)
        );
        let _ = writeln!(
            s,
            "  idle_musings           {}",
            on_off(self.idle_musings_enabled)
        );
        let _ = writeln!(
            s,
            "  conversation_reentry   {}",
            on_off(self.conversation_reentry_enabled)
        );
        let _ = writeln!(
            s,
            "  ticket_intake          {}",
            on_off(self.ticket_intake_enabled)
        );
        let _ = writeln!(s, "  reentry.delay          {}s", self.reentry_delay_secs);
        let _ = writeln!(s, "  reentry.jitter         {:.2}", self.reentry_jitter);
        let _ = writeln!(
            s,
            "  relationship_tracking  {}",
            on_off(self.relationship_tracking_enabled)
        );
        let _ = writeln!(
            s,
            "  mood_injection         {}",
            on_off(self.mood_injection_enabled)
        );
        let _ = writeln!(s, "  spontaneity            {:.2}", self.spontaneity);
        let _ = writeln!(
            s,
            "  heartbeat.interval     {}s",
            self.heartbeat_interval_secs
        );
        let _ = writeln!(s, "  heartbeat.jitter       {:.2}", self.heartbeat_jitter);
        let _ = writeln!(
            s,
            "  memory_scan.interval   {}s",
            self.memory_scan_interval_secs
        );
        let _ = writeln!(s, "  idle_threshold         {}s", self.idle_threshold_secs);
        let _ = writeln!(
            s,
            "  ticket_intake.interval {}s",
            self.ticket_intake_interval_secs
        );
        let _ = writeln!(
            s,
            "  mood.drift_interval    {}s",
            self.mood_drift_interval_secs
        );
        let _ = writeln!(s, "  tolerance              {:.2}", self.pulse_tolerance);
        let _ = writeln!(
            s,
            "  quiet_hours            {}",
            match self.quiet_hours {
                Some((start, end)) => format!("{}-{}", start, end),
                None => "off".to_string(),
            }
        );
        let _ = writeln!(s, "  timezone_offset        {}h", self.timezone_offset);
        let _ = writeln!(s, "  cli_tool               {}", self.cli_tool);
        let _ = writeln!(
            s,
            "  cli_model              {}",
            if self.cli_model.is_empty() {
                "default"
            } else {
                &self.cli_model
            }
        );
        let _ = writeln!(
            s,
            "  self_dev               {}",
            on_off(self.self_dev_enabled)
        );
        let _ = writeln!(
            s,
            "  metrics.interval       {}s",
            self.metrics_interval_secs
        );
        let _ = writeln!(
            s,
            "  code_indexer           {}",
            on_off(self.code_indexer_enabled)
        );
        let _ = writeln!(
            s,
            "  code_indexer.interval  {}s",
            self.code_indexer_interval_secs
        );
        let _ = writeln!(
            s,
            "  refactoring_scan       {}",
            on_off(self.refactoring_scan_enabled)
        );
        let _ = writeln!(
            s,
            "  refactoring_scan.interval {}s",
            self.refactoring_scan_interval_secs
        );
        let _ = writeln!(
            s,
            "  langfuse               {}",
            on_off(self.langfuse_enabled)
        );
        s
    }
}

pub type SharedKnobs = Arc<RwLock<RuntimeKnobs>>;

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err(format!("Expected on/off, got: {}", s)),
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}
