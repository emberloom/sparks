use std::path::PathBuf;

use chrono::{SecondsFormat, Utc};
use rusqlite::Connection;
use serde::Serialize;

use crate::config::{self, Config};
use crate::docker;
use crate::knobs;
use crate::profiles;
use crate::reason_codes;
use crate::secrets;
use crate::tool_usage::ToolUsageStore;
use crate::tools;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }
}

#[derive(Clone)]
struct CheckItem {
    stage: &'static str,
    status: CheckStatus,
    detail: String,
    fix: Option<String>,
}

struct FunnelReport {
    name: &'static str,
    checks: Vec<CheckItem>,
}

impl FunnelReport {
    fn worst_status(&self) -> CheckStatus {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(CheckStatus::Pass)
    }

    fn break_point(&self) -> Option<&CheckItem> {
        self.checks
            .iter()
            .find(|c| c.status == CheckStatus::Fail)
            .or_else(|| self.checks.iter().find(|c| c.status == CheckStatus::Warn))
    }
}

#[derive(Clone)]
struct LlmHealth {
    status: CheckStatus,
    detail: String,
    fix: Option<String>,
}

const SECURITY_ATTESTATION_SCHEMA_VERSION: &str = "v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum JsonCheckStatus {
    Pass,
    Warn,
    Fail,
}

impl JsonCheckStatus {
    fn label(self) -> &'static str {
        match self {
            JsonCheckStatus::Pass => "PASS",
            JsonCheckStatus::Warn => "WARN",
            JsonCheckStatus::Fail => "FAIL",
        }
    }

    fn as_check_status(self) -> CheckStatus {
        match self {
            JsonCheckStatus::Pass => CheckStatus::Pass,
            JsonCheckStatus::Warn => CheckStatus::Warn,
            JsonCheckStatus::Fail => CheckStatus::Fail,
        }
    }
}

impl From<CheckStatus> for JsonCheckStatus {
    fn from(value: CheckStatus) -> Self {
        match value {
            CheckStatus::Pass => JsonCheckStatus::Pass,
            CheckStatus::Warn => JsonCheckStatus::Warn,
            CheckStatus::Fail => JsonCheckStatus::Fail,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SecurityAttestationReport {
    schema_version: String,
    generated_at: String,
    summary: SecuritySummary,
    ghosts: Vec<GhostSecurityReport>,
}

#[derive(Debug, Clone, Serialize)]
struct SecuritySummary {
    overall_status: JsonCheckStatus,
    ghosts_total: usize,
    ghosts_failing: usize,
    checks_pass: usize,
    checks_warn: usize,
    checks_fail: usize,
}

#[derive(Debug, Clone, Serialize)]
struct GhostSecurityReport {
    name: String,
    status: JsonCheckStatus,
    effective_controls: EffectiveControls,
    checks: Vec<SecurityCheck>,
}

#[derive(Debug, Clone, Serialize)]
struct EffectiveControls {
    container_mode: String,
    caps_dropped: Vec<String>,
    rootfs_readonly: bool,
    network_mode: String,
    pid_limit: i64,
    memory_limit: i64,
    cpu_quota: i64,
    tool_guard: bool,
    path_guard: bool,
    sensitive_pattern_guard: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SecurityCheck {
    id: String,
    status: JsonCheckStatus,
    observed: String,
    expected: String,
    remediation: Option<String>,
}

#[derive(Debug, Clone)]
struct SecurityContext {
    container: docker::EffectiveContainerSecurity,
    valid_sensitive_patterns: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostCompilePreflight {
    pub reason_code: Option<&'static str>,
    pub detail: String,
    pub remediation: Option<String>,
    pub rg_available: bool,
}

impl GhostCompilePreflight {
    pub fn is_ok(&self) -> bool {
        self.reason_code.is_none()
    }

    pub fn failure_message(&self, ghost_name: &str) -> Option<String> {
        let reason = self.reason_code?;
        let mut message = reason_codes::with_reason(
            reason,
            format!(
                "Ghost '{}' compile/runtime preflight failed: {}",
                ghost_name, self.detail
            ),
        );
        if let Some(remediation) = &self.remediation {
            message.push_str(&format!(" Remediation: {}", remediation));
        }
        Some(message)
    }

    pub fn rg_fallback_message(&self, ghost_name: &str) -> Option<String> {
        if self.is_ok() && !self.rg_available {
            Some(format!(
                "Ghost '{}' does not provide ripgrep (`rg`). `rg --files ...` will use `find ... -type f`; other rg patterns fail fast with a reason tag.",
                ghost_name
            ))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CompileRuntimeProbe {
    missing_bins: Vec<String>,
    write_fail_paths: Vec<String>,
    cargo_version: Option<String>,
    rg_available: bool,
}

const COMPILE_RUNTIME_PROBE_CMD: &str = r#"
for bin in sh cargo; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "__SPARKS_MISSING_BIN__:$bin"
  fi
done
if command -v cargo >/dev/null 2>&1; then
  cargo --version 2>/dev/null | sed 's/^/__SPARKS_CARGO_VERSION__:/'
fi
if command -v rg >/dev/null 2>&1; then
  echo "__SPARKS_RG__:1"
else
  echo "__SPARKS_RG__:0"
fi
for dir in "${TMPDIR:-/tmp}" "${CARGO_HOME:-/tmp/cargo-home}" "${RUSTUP_HOME:-/tmp/rustup-home}" "${RUSTUP_HOME:-/tmp/rustup-home}/tmp"; do
  if ! mkdir -p "$dir" 2>/dev/null; then
    echo "__SPARKS_WRITE_FAIL__:$dir"
    continue
  fi
  probe="$dir/.sparks-write-probe.$$"
  if ! ( : > "$probe" ) 2>/dev/null; then
    echo "__SPARKS_WRITE_FAIL__:$dir"
    continue
  fi
  rm -f "$probe" 2>/dev/null || true
done
"#;
const SELF_DEV_TRUSTED_MIN_TIMEOUT_SECS: u64 = 600;
const SELF_DEV_TRUSTED_MIN_MAX_STEPS: usize = 30;

struct DoctorSnapshot {
    db_path: PathBuf,
    scout: Option<config::GhostConfig>,
    coder: Option<config::GhostConfig>,
    total_tool_calls: u64,
    failing_tools_len: usize,
    conversations: i64,
    counts: MemoryCounts,
    declared_cli_tools: Vec<String>,
    installed_cli_tools: Vec<String>,
    preferred_cli: String,
    rust_workspace: bool,
    inline_secret_labels: Vec<String>,
    keyring_missing: Vec<String>,
    keyring_error: Option<String>,
}

struct MemoryCounts {
    health_alert: i64,
    health_fix: i64,
    code_structure: i64,
    refactoring_opportunity: i64,
    refactoring_done: i64,
    refactoring_failed: i64,
    pattern: i64,
    musing: i64,
    heartbeat: i64,
    code_change: i64,
    code_change_failed: i64,
}

fn count_memories_by_category(conn: &Connection, category: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE active = 1 AND category = ?1",
        rusqlite::params![category],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn count_conversations(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap_or(0)
}

fn command_available(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return true;
                }
            }
        }
        #[cfg(not(unix))]
        {
            return true;
        }
    }

    false
}

fn has_tool(ghost: &config::GhostConfig, tool: &str) -> bool {
    ghost.tools.iter().any(|t| t == tool)
}

fn collect_memory_counts(conn: &Connection) -> MemoryCounts {
    MemoryCounts {
        health_alert: count_memories_by_category(conn, "health_alert"),
        health_fix: count_memories_by_category(conn, "health_fix"),
        code_structure: count_memories_by_category(conn, "code_structure"),
        refactoring_opportunity: count_memories_by_category(conn, "refactoring_opportunity"),
        refactoring_done: count_memories_by_category(conn, "refactoring_done"),
        refactoring_failed: count_memories_by_category(conn, "refactoring_failed"),
        pattern: count_memories_by_category(conn, "pattern"),
        musing: count_memories_by_category(conn, "musing"),
        heartbeat: count_memories_by_category(conn, "heartbeat"),
        code_change: count_memories_by_category(conn, "code_change"),
        code_change_failed: count_memories_by_category(conn, "code_change_failed"),
    }
}

fn collect_declared_cli_tools(coder: Option<&config::GhostConfig>) -> Vec<String> {
    coder
        .map(|g| {
            ["claude_code", "codex", "opencode"]
                .iter()
                .copied()
                .filter(|t| has_tool(g, t))
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn collect_installed_cli_tools() -> Vec<String> {
    [
        ("claude_code", "claude"),
        ("codex", "codex"),
        ("opencode", "opencode"),
    ]
    .iter()
    .filter_map(|(tool, bin)| command_available(bin).then_some((*tool).to_string()))
    .collect()
}

fn collect_inline_secret_labels(config: &Config) -> Vec<String> {
    config.inline_secret_labels().to_vec()
}

fn llm_check(status: &LlmHealth) -> CheckItem {
    CheckItem {
        stage: "LLM provider health",
        status: status.status,
        detail: status.detail.clone(),
        fix: status.fix.clone(),
    }
}

fn health_signal_check(snap: &DoctorSnapshot) -> CheckItem {
    CheckItem {
        stage: "Health signal data",
        status: if snap.total_tool_calls == 0 {
            CheckStatus::Warn
        } else {
            CheckStatus::Pass
        },
        detail: format!(
            "tool calls recorded={} failing_tools(>30%)={}",
            snap.total_tool_calls, snap.failing_tools_len
        ),
        fix: (snap.total_tool_calls == 0).then(|| {
            "Run a few real tasks so tool usage stats can drive anomaly detection.".to_string()
        }),
    }
}

fn feedback_loop_check(snap: &DoctorSnapshot) -> CheckItem {
    let feedback_ok = (snap.counts.health_alert + snap.counts.health_fix) > 0;
    CheckItem {
        stage: "Feedback memory loop",
        status: if feedback_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "health_alert={} health_fix={}",
            snap.counts.health_alert, snap.counts.health_fix
        ),
        fix: (!feedback_ok).then(|| {
            "After enabling self-dev, inspect observer logs for anomalies and confirm health memories get stored.".to_string()
        }),
    }
}

fn credentials_hygiene_check(snap: &DoctorSnapshot) -> CheckItem {
    let mut detail = if snap.inline_secret_labels.is_empty() {
        "No inline credentials found in config".to_string()
    } else {
        format!(
            "Inline secrets configured for: {}",
            snap.inline_secret_labels.join(", ")
        )
    };

    if let Some(err) = snap.keyring_error.as_ref() {
        detail.push_str(&format!(" | Keyring unavailable: {}", err));
    } else if !snap.keyring_missing.is_empty() {
        detail.push_str(&format!(
            " | Keyring missing: {}",
            snap.keyring_missing.join(", ")
        ));
    }

    CheckItem {
        stage: "Credentials storage hygiene",
        status: if snap.inline_secret_labels.is_empty() && snap.keyring_error.is_none() {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail,
        fix: (!snap.inline_secret_labels.is_empty() || !snap.keyring_missing.is_empty())
            .then(|| {
                "Move secrets into env vars, a .env file (gitignored), or use `sparks secrets set <key>`."
                    .to_string()
            }),
    }
}

fn collect_snapshot(config: &Config) -> anyhow::Result<DoctorSnapshot> {
    let db_path = config.db_path()?;
    let conn = Connection::open(&db_path)?;
    let usage_store = ToolUsageStore::new(Connection::open(&db_path)?);
    let ghosts = profiles::load_ghosts(config)?;

    let scout = ghosts.iter().find(|g| g.name == "scout").cloned();
    let coder = ghosts.iter().find(|g| g.name == "coder").cloned();

    let tool_stats = usage_store.all().unwrap_or_default();
    let total_tool_calls: u64 = tool_stats.iter().map(|s| s.invocation_count).sum();
    let failing_tools_len = usage_store.failing_tools(0.3).unwrap_or_default().len();
    let counts = collect_memory_counts(&conn);
    let declared_cli_tools = collect_declared_cli_tools(coder.as_ref());
    let installed_cli_tools = collect_installed_cli_tools();

    let runtime_knobs = knobs::RuntimeKnobs::from_config(config);
    let inline_secret_labels = collect_inline_secret_labels(config);
    let keyring_report = secrets::keyring_report();
    let keyring_missing = keyring_report
        .statuses
        .iter()
        .filter(|s| !s.in_env && !s.in_keyring)
        .map(|s| s.key.to_string())
        .collect::<Vec<_>>();

    Ok(DoctorSnapshot {
        db_path,
        scout,
        coder,
        total_tool_calls,
        failing_tools_len,
        conversations: count_conversations(&conn),
        counts,
        declared_cli_tools,
        installed_cli_tools,
        preferred_cli: runtime_knobs.cli_tool,
        rust_workspace: std::path::Path::new("Cargo.toml").exists(),
        inline_secret_labels,
        keyring_missing,
        keyring_error: keyring_report.error,
    })
}

async fn evaluate_llm_health(config: &Config, skip_llm: bool) -> LlmHealth {
    if skip_llm {
        return LlmHealth {
            status: CheckStatus::Pass,
            detail: "skipped (--skip-llm)".to_string(),
            fix: None,
        };
    }

    let provider_candidates = if config.local_only_enabled() {
        vec![config.llm.provider.clone()]
    } else {
        config.provider_candidates()
    };

    let mut failures = Vec::new();
    for provider_name in provider_candidates {
        match config.build_llm_provider_for(&provider_name) {
            Ok(provider) => match provider.health_check().await {
                Ok(()) => {
                    return LlmHealth {
                        status: CheckStatus::Pass,
                        detail: format!("{} reachable (selected)", provider.provider_name()),
                        fix: None,
                    };
                }
                Err(e) => failures.push(format!("{}: {}", provider_name, e)),
            },
            Err(e) => failures.push(format!("{}: {}", provider_name, e)),
        }
    }

    LlmHealth {
        status: CheckStatus::Fail,
        detail: format!("No reachable providers. Tried: {}", failures.join(" | ")),
        fix: Some(
            "Fix LLM connectivity (credentials/network) or switch to a reachable provider."
                .to_string(),
        ),
    }
}

fn build_funnel1(config: &Config, snap: &DoctorSnapshot, llm: &LlmHealth) -> FunnelReport {
    let mut checks = Vec::new();
    checks.push(llm_check(llm));

    let enabled = config.proactive.enabled && config.self_dev.enabled;
    checks.push(CheckItem {
        stage: "Health monitor enabled",
        status: if enabled {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "proactive.enabled={} self_dev.enabled={}",
            config.proactive.enabled, config.self_dev.enabled
        ),
        fix: (!enabled).then(|| {
            "Enable both `[proactive].enabled = true` and `[self_dev].enabled = true`.".to_string()
        }),
    });

    let scout_ok = snap
        .scout
        .as_ref()
        .map(|g| has_tool(g, "file_read") && has_tool(g, "shell"))
        .unwrap_or(false);
    let health_monitor_requested = config.proactive.enabled || config.self_dev.enabled;
    checks.push(CheckItem {
        stage: "Diagnostic ghost wiring",
        status: if scout_ok {
            CheckStatus::Pass
        } else if health_monitor_requested {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: if scout_ok {
            "scout ghost exists and has file_read + shell".to_string()
        } else {
            "scout ghost missing or lacks required tools".to_string()
        },
        fix: (!scout_ok).then(|| {
            "Ensure `scout` exists with at least `file_read` and `shell` tools.".to_string()
        }),
    });

    checks.push(health_signal_check(snap));
    checks.push(feedback_loop_check(snap));
    checks.push(credentials_hygiene_check(snap));

    FunnelReport {
        name: "Funnel 1: Health Monitor -> Diagnose -> Auto-Fix",
        checks,
    }
}

fn code_indexer_enabled_check(config: &Config) -> CheckItem {
    let index_enabled = config.self_dev.enabled && config.self_dev.code_indexer_enabled;
    CheckItem {
        stage: "Code indexer enabled",
        status: if index_enabled {
            CheckStatus::Pass
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "self_dev.enabled={} code_indexer_enabled={}",
            config.self_dev.enabled, config.self_dev.code_indexer_enabled
        ),
        fix: (!index_enabled).then(|| {
            "Set `[self_dev].enabled = true` and `[self_dev].code_indexer_enabled = true`."
                .to_string()
        }),
    }
}

fn refactor_scanner_enabled_check(config: &Config) -> CheckItem {
    let refactor_enabled = config.self_dev.enabled && config.self_dev.refactoring_scan_enabled;
    CheckItem {
        stage: "Refactoring scanner enabled",
        status: if refactor_enabled {
            CheckStatus::Pass
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "self_dev.enabled={} refactoring_scan_enabled={}",
            config.self_dev.enabled, config.self_dev.refactoring_scan_enabled
        ),
        fix: (!refactor_enabled)
            .then(|| "Set `[self_dev].refactoring_scan_enabled = true`.".to_string()),
    }
}

fn refactor_wiring_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    let coder_ok = snap
        .coder
        .as_ref()
        .map(|g| g.strategy == "code" && has_tool(g, "file_read"))
        .unwrap_or(false);
    let ready = snap.scout.is_some() && coder_ok;
    CheckItem {
        stage: "Indexer/refactor ghost wiring",
        status: if ready {
            CheckStatus::Pass
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: if ready {
            "scout + coder(strategy=code) present".to_string()
        } else {
            "missing scout or coder(strategy=code)".to_string()
        },
        fix: (snap.scout.is_none() || !coder_ok).then(|| {
            "Configure both `scout` and `coder` ghosts, with coder using `strategy = \"code\"`."
                .to_string()
        }),
    }
}

fn index_artifacts_check(snap: &DoctorSnapshot) -> CheckItem {
    CheckItem {
        stage: "Index artifacts present",
        status: if snap.counts.code_structure > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!("code_structure memories={}", snap.counts.code_structure),
        fix: (snap.counts.code_structure == 0).then(|| {
            "Run Sparks with self-dev enabled long enough for the code indexer interval to elapse."
                .to_string()
        }),
    }
}

fn refactor_artifacts_check(snap: &DoctorSnapshot) -> CheckItem {
    let refactor_artifacts = snap.counts.refactoring_opportunity
        + snap.counts.refactoring_done
        + snap.counts.refactoring_failed;
    CheckItem {
        stage: "Refactor analysis artifacts present",
        status: if refactor_artifacts > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "refactoring_opportunity={} done={} failed={}",
            snap.counts.refactoring_opportunity,
            snap.counts.refactoring_done,
            snap.counts.refactoring_failed
        ),
        fix: (refactor_artifacts == 0).then(|| {
            "Wait for the refactoring scanner cycle or trigger related autonomous tasks."
                .to_string()
        }),
    }
}

fn build_funnel2(config: &Config, snap: &DoctorSnapshot, llm: &LlmHealth) -> FunnelReport {
    let mut checks = Vec::new();
    checks.push(llm_check(llm));
    checks.push(code_indexer_enabled_check(config));
    checks.push(refactor_scanner_enabled_check(config));
    checks.push(refactor_wiring_check(config, snap));
    checks.push(index_artifacts_check(snap));
    checks.push(refactor_artifacts_check(snap));

    FunnelReport {
        name: "Funnel 2: Index -> Analyze -> Propose -> Refactor",
        checks,
    }
}

fn build_funnel3(config: &Config, snap: &DoctorSnapshot, llm: &LlmHealth) -> FunnelReport {
    let mut checks = Vec::new();
    checks.push(llm_check(llm));

    checks.push(CheckItem {
        stage: "Conversation ingestion",
        status: if snap.conversations > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!("conversation turns={}", snap.conversations),
        fix: (snap.conversations == 0).then(|| {
            "Have at least one chat session so memory scanner/heartbeat have material.".to_string()
        }),
    });

    let loops_enabled = config.proactive.enabled && config.heartbeat.enabled;
    let loops_partial = config.proactive.enabled || config.heartbeat.enabled;
    checks.push(CheckItem {
        stage: "Reflection loops enabled",
        status: if loops_enabled {
            CheckStatus::Pass
        } else if loops_partial {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "proactive.enabled={} heartbeat.enabled={}",
            config.proactive.enabled, config.heartbeat.enabled
        ),
        fix: (!loops_enabled).then(|| {
            "Enable `[proactive].enabled = true` and `[heartbeat].enabled = true`.".to_string()
        }),
    });

    checks.push(CheckItem {
        stage: "Stochastic settings sanity",
        status: if (0.05..=0.95).contains(&config.proactive.spontaneity)
            && (0.0..=1.0).contains(&config.initiative.tolerance)
        {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "spontaneity={:.2} tolerance={:.2}",
            config.proactive.spontaneity, config.initiative.tolerance
        ),
        fix: None,
    });

    let reflection_outputs = snap.counts.pattern + snap.counts.musing + snap.counts.heartbeat;
    checks.push(CheckItem {
        stage: "Reflection outputs present",
        status: if reflection_outputs > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "pattern={} musing={} heartbeat={}",
            snap.counts.pattern, snap.counts.musing, snap.counts.heartbeat
        ),
        fix: (reflection_outputs == 0).then(|| {
            "Let proactive loops run for at least one full interval (heartbeat + memory scan + idle).".to_string()
        }),
    });

    FunnelReport {
        name: "Funnel 3: Interact -> Learn -> Evolve",
        checks,
    }
}

async fn build_funnel4(config: &Config, snap: &DoctorSnapshot, llm: &LlmHealth) -> FunnelReport {
    let mut checks = Vec::new();
    checks.push(llm_check(llm));
    checks.push(coder_strategy_check(config, snap));
    checks.push(verify_tool_coverage_check(config, snap));
    checks.push(self_heal_tool_coverage_check(snap));

    if needs_cargo_check(snap) {
        checks.push(run_cargo_check(config, snap).await);
    }
    checks.push(coding_cli_declaration_check(config, snap));
    checks.push(coding_cli_availability_check(config, snap));
    checks.push(preferred_cli_viability_check(snap));
    checks.push(execution_learning_check(snap));

    FunnelReport {
        name: "Funnel 4: Execute -> Verify -> Self-Heal -> Learn",
        checks,
    }
}

fn coder_strategy_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    let coder_is_code = snap
        .coder
        .as_ref()
        .map(|g| g.strategy == "code")
        .unwrap_or(false);
    CheckItem {
        stage: "Coder strategy wiring",
        status: if coder_is_code {
            CheckStatus::Pass
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: if coder_is_code {
            "coder ghost uses strategy=code".to_string()
        } else {
            "coder missing or strategy != code".to_string()
        },
        fix: (!coder_is_code).then(|| "Set `coder.strategy = \"code\"`.".to_string()),
    }
}

fn verify_tool_coverage_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    let coder_tools_ok = snap
        .coder
        .as_ref()
        .map(|g| {
            ["file_read", "grep", "glob", "shell", "lint", "diff"]
                .iter()
                .all(|t| has_tool(g, t))
        })
        .unwrap_or(false);
    CheckItem {
        stage: "Execute/verify tool coverage",
        status: if coder_tools_ok {
            CheckStatus::Pass
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: if coder_tools_ok {
            "required read/verify tools present".to_string()
        } else {
            "coder missing one or more required tools (file_read, grep, glob, shell, lint, diff)"
                .to_string()
        },
        fix: (!coder_tools_ok).then(|| "Add missing verify tools to coder ghost.".to_string()),
    }
}

fn self_heal_tool_coverage_check(snap: &DoctorSnapshot) -> CheckItem {
    let test_heal_tools_ok = snap
        .coder
        .as_ref()
        .map(|g| {
            ["file_write", "file_edit", "test_runner"]
                .iter()
                .all(|t| has_tool(g, t))
        })
        .unwrap_or(false);
    CheckItem {
        stage: "Self-heal tool coverage",
        status: if test_heal_tools_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: if test_heal_tools_ok {
            "file_write + file_edit + test_runner present".to_string()
        } else {
            "self-heal may be limited without file_write/file_edit/test_runner".to_string()
        },
        fix: (!test_heal_tools_ok).then(|| {
            "Add `file_write`, `file_edit`, and `test_runner` to coder for full self-heal flow."
                .to_string()
        }),
    }
}

fn coding_cli_declaration_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    CheckItem {
        stage: "Coding CLI declaration",
        status: if snap.declared_cli_tools.is_empty() {
            if config.self_dev.enabled {
                CheckStatus::Fail
            } else {
                CheckStatus::Warn
            }
        } else {
            CheckStatus::Pass
        },
        detail: format!(
            "declared coding tools={}",
            snap.declared_cli_tools.join(", ")
        ),
        fix: snap.declared_cli_tools.is_empty().then(|| {
            "Add at least one of `claude_code`, `codex`, `opencode` to coder tools.".to_string()
        }),
    }
}

fn coding_cli_availability_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    let usable_declared = snap
        .declared_cli_tools
        .iter()
        .filter(|t| snap.installed_cli_tools.contains(t))
        .count();
    CheckItem {
        stage: "Coding CLI availability",
        status: if usable_declared > 0 {
            CheckStatus::Pass
        } else if snap.declared_cli_tools.is_empty() {
            CheckStatus::Warn
        } else if config.self_dev.enabled {
            CheckStatus::Fail
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "installed coding tools={} (detected: {})",
            usable_declared,
            snap.installed_cli_tools.join(", ")
        ),
        fix: (usable_declared == 0).then(|| {
            "Install at least one declared coding CLI and ensure it is on PATH.".to_string()
        }),
    }
}

fn preferred_cli_viability_check(snap: &DoctorSnapshot) -> CheckItem {
    let preferred_ok = snap
        .installed_cli_tools
        .iter()
        .any(|t| t == &snap.preferred_cli);
    CheckItem {
        stage: "Preferred CLI viability",
        status: if preferred_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!("preferred cli_tool={}", snap.preferred_cli),
        fix: (!preferred_ok)
            .then(|| "Switch with `/set cli_tool codex` (or another installed tool).".to_string()),
    }
}

fn execution_learning_check(snap: &DoctorSnapshot) -> CheckItem {
    let learning_artifacts = snap.counts.code_change + snap.counts.code_change_failed;
    CheckItem {
        stage: "Execution learning artifacts",
        status: if learning_artifacts > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: format!(
            "code_change={} code_change_failed={}",
            snap.counts.code_change, snap.counts.code_change_failed
        ),
        fix: (learning_artifacts == 0).then(|| {
            "Run one coder task end-to-end and confirm completion/failure memories are stored."
                .to_string()
        }),
    }
}

async fn build_local_only_funnel(config: &Config, skip_llm: bool) -> FunnelReport {
    let checks = vec![
        local_only_profile_check(config),
        local_only_provider_check(config),
        local_only_ollama_endpoint_check(config),
        local_only_storage_paths_check(config),
        local_only_outbound_integrations_check(config),
        local_only_ollama_reachability_check(config, skip_llm).await,
    ];

    FunnelReport {
        name: "Funnel 5: Local-Only Deployment Readiness",
        checks,
    }
}

fn local_only_not_enforced(stage: &'static str, detail: String) -> CheckItem {
    CheckItem {
        stage,
        status: CheckStatus::Pass,
        detail,
        fix: None,
    }
}

fn local_only_profile_check(config: &Config) -> CheckItem {
    let profile = config.runtime_profile_name();
    CheckItem {
        stage: "Runtime profile selection",
        status: CheckStatus::Pass,
        detail: if config.local_only_enabled() {
            format!("runtime.profile={profile} (strict local-only checks enabled)")
        } else {
            format!("runtime.profile={profile} (local-only checks are informational)")
        },
        fix: (!config.local_only_enabled()).then(|| {
            "Set `[runtime].profile = \"local_only\"` for strict local execution checks."
                .to_string()
        }),
    }
}

fn local_only_provider_check(config: &Config) -> CheckItem {
    if !config.local_only_enabled() {
        return local_only_not_enforced(
            "Local model provider",
            format!(
                "provider={} (runtime.profile != local_only)",
                config.llm.provider
            ),
        );
    }

    let provider_ok = config.llm.provider == "ollama";
    CheckItem {
        stage: "Local model provider",
        status: if provider_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: format!("llm.provider={}", config.llm.provider),
        fix: (!provider_ok).then(|| {
            "Set `[llm].provider = \"ollama\"` when `[runtime].profile = \"local_only\"`."
                .to_string()
        }),
    }
}

fn local_only_ollama_endpoint_check(config: &Config) -> CheckItem {
    if !config.local_only_enabled() {
        return local_only_not_enforced(
            "Ollama endpoint loopback",
            format!(
                "ollama.url={} (runtime.profile != local_only)",
                config.ollama.url
            ),
        );
    }

    let host = config
        .ollama_url_host()
        .unwrap_or_else(|| "<invalid-url>".to_string());
    let endpoint_ok = config.ollama_url_is_loopback();

    CheckItem {
        stage: "Ollama endpoint loopback",
        status: if endpoint_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: format!("ollama.url={} (host={host})", config.ollama.url),
        fix: (!endpoint_ok).then(|| {
            "Use a loopback Ollama URL (e.g. `http://localhost:11434` or `http://127.0.0.1:11434`)."
                .to_string()
        }),
    }
}

fn local_only_storage_paths_check(config: &Config) -> CheckItem {
    if !config.local_only_enabled() {
        return local_only_not_enforced(
            "Local storage paths",
            format!(
                "db.path={} embedding.model_dir={} (runtime.profile != local_only)",
                config.db.path, config.embedding.model_dir
            ),
        );
    }

    let db_local = is_local_path(&config.db.path) && config.db_path().is_ok();
    let model_local =
        is_local_path(&config.embedding.model_dir) && config.resolve_model_dir().is_ok();
    let paths_ok = db_local && model_local;

    CheckItem {
        stage: "Local storage paths",
        status: if paths_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: format!(
            "db.path={} embedding.model_dir={}",
            config.db.path, config.embedding.model_dir
        ),
        fix: (!paths_ok).then(|| {
            "Use filesystem paths (for example `~/.sparks/sparks.db` and `~/.sparks/models/all-MiniLM-L6-v2`)."
                .to_string()
        }),
    }
}

fn local_only_outbound_integrations_check(config: &Config) -> CheckItem {
    if !config.local_only_enabled() {
        return local_only_not_enforced(
            "Outbound integration toggles",
            "runtime.profile != local_only".to_string(),
        );
    }

    let mut outbound_risks = Vec::new();
    if config.langfuse.enabled
        || config.langfuse.public_key.is_some()
        || config.langfuse.secret_key.is_some()
    {
        outbound_risks.push("langfuse");
    }
    if config.ticket_intake.enabled {
        outbound_risks.push("ticket_intake.enabled");
    }
    if !config.ticket_intake.sources.is_empty() {
        outbound_risks.push("ticket_intake.sources");
    }
    if config.ticket_intake.webhook.enabled {
        outbound_risks.push("ticket_intake.webhook.enabled");
    }

    let outbound_clean = outbound_risks.is_empty();
    CheckItem {
        stage: "Outbound integration toggles",
        status: if outbound_clean {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if outbound_clean {
            "langfuse + ticket intake integrations are disabled".to_string()
        } else {
            format!("potential outbound paths enabled: {}", outbound_risks.join(", "))
        },
        fix: (!outbound_clean).then(|| {
            "Disable `[langfuse]`, set `[ticket_intake].enabled = false`, keep `[ticket_intake].sources = []`, and disable `[ticket_intake.webhook]` for local-only mode."
                .to_string()
        }),
    }
}

async fn local_only_ollama_reachability_check(config: &Config, skip_llm: bool) -> CheckItem {
    if !config.local_only_enabled() {
        return local_only_not_enforced(
            "Local Ollama reachability",
            "runtime.profile != local_only".to_string(),
        );
    }

    if skip_llm {
        return CheckItem {
            stage: "Local Ollama reachability",
            status: CheckStatus::Pass,
            detail: "skipped (--skip-llm)".to_string(),
            fix: None,
        };
    }

    if config.llm.provider != "ollama" {
        return CheckItem {
            stage: "Local Ollama reachability",
            status: CheckStatus::Warn,
            detail: format!(
                "provider={} (skip direct Ollama probe)",
                config.llm.provider
            ),
            fix: Some("Set `[llm].provider = \"ollama\"` for local-only mode.".to_string()),
        };
    }

    if !config.ollama_url_is_loopback() {
        return CheckItem {
            stage: "Local Ollama reachability",
            status: CheckStatus::Warn,
            detail: format!(
                "ollama.url={} is not loopback (probe skipped to avoid outbound request)",
                config.ollama.url
            ),
            fix: Some("Set `[ollama].url` to a loopback address and rerun doctor.".to_string()),
        };
    }

    match config.build_llm_provider_for("ollama") {
        Ok(provider) => match provider.health_check().await {
            Ok(()) => CheckItem {
                stage: "Local Ollama reachability",
                status: CheckStatus::Pass,
                detail: format!("{} reachable", config.ollama.url),
                fix: None,
            },
            Err(e) => CheckItem {
                stage: "Local Ollama reachability",
                status: CheckStatus::Fail,
                detail: format!("{} unreachable: {}", config.ollama.url, e),
                fix: Some(
                    "Start/restart the local Ollama daemon and pull the configured model."
                        .to_string(),
                ),
            },
        },
        Err(e) => CheckItem {
            stage: "Local Ollama reachability",
            status: CheckStatus::Fail,
            detail: format!("failed to build Ollama provider: {}", e),
            fix: Some("Fix `[ollama]` config and rerun doctor.".to_string()),
        },
    }
}

fn self_dev_trusted_not_selected(stage: &'static str, detail: String) -> CheckItem {
    CheckItem {
        stage,
        status: CheckStatus::Pass,
        detail,
        fix: None,
    }
}

fn self_dev_runtime_profile_check(config: &Config) -> CheckItem {
    let profile = config.runtime_profile_name();
    CheckItem {
        stage: "Self-dev runtime profile",
        status: CheckStatus::Pass,
        detail: if config.self_dev_trusted_enabled() {
            format!(
                "runtime.profile={profile} (trusted host execution allowed for allowlisted repos)"
            )
        } else {
            format!("runtime.profile={profile} (container isolation remains enforced)")
        },
        fix: (!config.self_dev_trusted_enabled()).then(|| {
            "Set `[runtime].profile = \"self_dev_trusted\"` to allow trusted host execution."
                .to_string()
        }),
    }
}

fn self_dev_trusted_enablement_check(config: &Config) -> CheckItem {
    if !config.self_dev_trusted_enabled() {
        return self_dev_trusted_not_selected(
            "Trusted mode enablement",
            "runtime.profile != self_dev_trusted".to_string(),
        );
    }

    CheckItem {
        stage: "Trusted mode enablement",
        status: if config.self_dev.enabled {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: format!("self_dev.enabled={}", config.self_dev.enabled),
        fix: (!config.self_dev.enabled)
            .then(|| "Set `[self_dev].enabled = true` for trusted self-dev mode.".to_string()),
    }
}

fn self_dev_trusted_repo_allowlist_check(config: &Config) -> CheckItem {
    if !config.self_dev_trusted_enabled() {
        return self_dev_trusted_not_selected(
            "Trusted repo allowlist",
            "runtime.profile != self_dev_trusted".to_string(),
        );
    }

    let repos = config.trusted_self_dev_repos();
    CheckItem {
        stage: "Trusted repo allowlist",
        status: if repos.is_empty() {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        detail: if repos.is_empty() {
            reason_codes::with_reason(
                reason_codes::REASON_SELF_DEV_MODE_RESTRICTION,
                "self_dev.trusted_repos is empty",
            )
        } else {
            format!("trusted repos: {}", repos.join(", "))
        },
        fix: repos.is_empty().then(|| {
            "Set `[self_dev].trusted_repos = [\"sparks\"]` (or your trusted repo names)."
                .to_string()
        }),
    }
}

fn self_dev_trusted_budget_check(config: &Config) -> CheckItem {
    if !config.self_dev_trusted_enabled() {
        return self_dev_trusted_not_selected(
            "Self-dev execution budgets",
            format!(
                "runtime.profile != self_dev_trusted (timeout={}s max_steps={})",
                config.docker.timeout_secs, config.manager.max_steps
            ),
        );
    }

    let timeout_ok = config.docker.timeout_secs >= SELF_DEV_TRUSTED_MIN_TIMEOUT_SECS;
    let steps_ok = config.manager.max_steps >= SELF_DEV_TRUSTED_MIN_MAX_STEPS;
    let budgets_ok = timeout_ok && steps_ok;

    CheckItem {
        stage: "Self-dev execution budgets",
        status: if budgets_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: format!(
            "docker.timeout_secs={} (min {}) manager.max_steps={} (min {})",
            config.docker.timeout_secs,
            SELF_DEV_TRUSTED_MIN_TIMEOUT_SECS,
            config.manager.max_steps,
            SELF_DEV_TRUSTED_MIN_MAX_STEPS
        ),
        fix: (!budgets_ok).then(|| {
            format!(
                "Set `[docker].timeout_secs >= {}` and `[manager].max_steps >= {}` for trusted self-dev workloads.",
                SELF_DEV_TRUSTED_MIN_TIMEOUT_SECS, SELF_DEV_TRUSTED_MIN_MAX_STEPS
            )
        }),
    }
}

async fn self_dev_trusted_compile_preflight_check(
    config: &Config,
    snap: &DoctorSnapshot,
) -> CheckItem {
    if !config.self_dev_trusted_enabled() {
        return self_dev_trusted_not_selected(
            "Trusted compile preflight",
            "runtime.profile != self_dev_trusted".to_string(),
        );
    }

    let Some(coder) = snap.coder.as_ref() else {
        return CheckItem {
            stage: "Trusted compile preflight",
            status: CheckStatus::Fail,
            detail: "coder ghost is missing".to_string(),
            fix: Some("Configure a `coder` ghost for trusted self-dev workflows.".to_string()),
        };
    };

    let preflight = run_ghost_compile_preflight(config, coder).await;
    match preflight.reason_code {
        None => CheckItem {
            stage: "Trusted compile preflight",
            status: CheckStatus::Pass,
            detail: preflight.detail,
            fix: None,
        },
        Some(_) => CheckItem {
            stage: "Trusted compile preflight",
            status: CheckStatus::Fail,
            detail: preflight.detail,
            fix: preflight.remediation,
        },
    }
}

async fn build_self_dev_runtime_funnel(config: &Config, snap: &DoctorSnapshot) -> FunnelReport {
    let mut checks = Vec::new();
    checks.push(self_dev_runtime_profile_check(config));
    checks.push(self_dev_trusted_enablement_check(config));
    checks.push(self_dev_trusted_repo_allowlist_check(config));
    checks.push(self_dev_trusted_budget_check(config));
    checks.push(self_dev_trusted_compile_preflight_check(config, snap).await);

    FunnelReport {
        name: "Funnel 6: Self-Dev Runtime Mode Readiness",
        checks,
    }
}

fn is_local_path(path: &str) -> bool {
    !path.contains("://")
}

fn parse_compile_runtime_probe(output: &str) -> CompileRuntimeProbe {
    let mut probe = CompileRuntimeProbe::default();

    for line in output.lines().map(str::trim) {
        if let Some(bin) = line.strip_prefix("__SPARKS_MISSING_BIN__:") {
            if !bin.is_empty() {
                probe.missing_bins.push(bin.to_string());
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("__SPARKS_WRITE_FAIL__:") {
            if !path.is_empty() {
                probe.write_fail_paths.push(path.to_string());
            }
            continue;
        }
        if let Some(version) = line.strip_prefix("__SPARKS_CARGO_VERSION__:") {
            let trimmed = version.trim();
            if !trimmed.is_empty() {
                probe.cargo_version = Some(trimmed.to_string());
            }
            continue;
        }
        if let Some(flag) = line.strip_prefix("__SPARKS_RG__:") {
            probe.rg_available = flag.trim() == "1";
        }
    }

    probe
}

fn evaluate_compile_runtime_probe(probe: CompileRuntimeProbe) -> GhostCompilePreflight {
    if !probe.missing_bins.is_empty() {
        return GhostCompilePreflight {
            reason_code: Some(reason_codes::REASON_GHOST_TOOL_UNAVAILABLE),
            detail: format!(
                "required tools missing in ghost container: {}",
                probe.missing_bins.join(", ")
            ),
            remediation: Some(
                "Use a ghost image that includes `/bin/sh` and Rust toolchain binaries (cargo/rustc)."
                    .to_string(),
            ),
            rg_available: probe.rg_available,
        };
    }

    if !probe.write_fail_paths.is_empty() {
        return GhostCompilePreflight {
            reason_code: Some(reason_codes::REASON_GHOST_RUST_TEMP_UNWRITABLE),
            detail: format!(
                "unwritable runtime dirs detected: {}",
                probe.write_fail_paths.join(", ")
            ),
            remediation: Some(
                "Configure writable TMPDIR/CARGO_HOME/RUSTUP_HOME paths in the ghost runtime (for example under /tmp)."
                    .to_string(),
            ),
            rg_available: probe.rg_available,
        };
    }

    let cargo_version = probe
        .cargo_version
        .unwrap_or_else(|| "cargo detected (version unavailable)".to_string());
    let rg_status = if probe.rg_available {
        "rg available".to_string()
    } else {
        "rg missing (fallback enabled for `rg --files`)".to_string()
    };

    GhostCompilePreflight {
        reason_code: None,
        detail: format!(
            "{cargo_version}; writable TMPDIR/CARGO_HOME/RUSTUP_HOME confirmed; {rg_status}"
        ),
        remediation: None,
        rg_available: probe.rg_available,
    }
}

pub async fn run_ghost_compile_preflight(
    config: &Config,
    ghost: &config::GhostConfig,
) -> GhostCompilePreflight {
    let trusted_repos = config.trusted_self_dev_repos();
    let trusted_policy = if config.self_dev_trusted_enabled() {
        Some(trusted_repos.as_slice())
    } else {
        None
    };
    let session = match docker::DockerSession::new(ghost, &config.docker, trusted_policy).await {
        Ok(session) => session,
        Err(e) => {
            return GhostCompilePreflight {
                reason_code: Some(reason_codes::REASON_GHOST_RUNTIME_CAPABILITY_MISMATCH),
                detail: format!("failed to start ghost runtime session: {}", e),
                remediation: Some(
                    "Fix ghost runtime configuration (container or trusted host mode), then rerun dispatch."
                        .to_string(),
                ),
                rg_available: false,
            };
        }
    };

    let mode = session.execution_mode().to_string();
    let probe = session.exec(COMPILE_RUNTIME_PROBE_CMD).await;
    if let Err(e) = session.close().await {
        tracing::warn!("Failed to close compile preflight container: {}", e);
    }

    match probe {
        Ok(output) => {
            let mut result = evaluate_compile_runtime_probe(parse_compile_runtime_probe(&output));
            result.detail = format!("mode={}; {}", mode, result.detail);
            result
        }
        Err(e) => GhostCompilePreflight {
            reason_code: Some(reason_codes::REASON_GHOST_RUNTIME_CAPABILITY_MISMATCH),
            detail: format!("compile runtime probe failed: {}", e),
            remediation: Some(
                "Fix ghost runtime startup/exec settings and rerun `sparks doctor --ci`."
                    .to_string(),
            ),
            rg_available: false,
        },
    }
}

fn needs_cargo_check(snap: &DoctorSnapshot) -> bool {
    snap.rust_workspace
        && snap
            .coder
            .as_ref()
            .map(|g| has_tool(g, "lint") || has_tool(g, "test_runner"))
            .unwrap_or(false)
}

async fn run_cargo_check(config: &Config, snap: &DoctorSnapshot) -> CheckItem {
    let Some(coder) = snap.coder.as_ref() else {
        return CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Fail,
            detail: "coder ghost is missing".to_string(),
            fix: Some("Configure a `coder` ghost for Rust tasks.".to_string()),
        };
    };

    let preflight = run_ghost_compile_preflight(config, coder).await;
    match preflight.reason_code {
        None => CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Pass,
            detail: preflight.detail,
            fix: None,
        },
        Some(_) => CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Fail,
            detail: preflight.detail,
            fix: preflight.remediation,
        },
    }
}

fn valid_sensitive_pattern_count(patterns: &[String]) -> usize {
    patterns
        .iter()
        .filter(|pattern| regex::Regex::new(pattern).is_ok())
        .count()
}

fn security_check(
    id: &str,
    status: CheckStatus,
    observed: String,
    expected: String,
    remediation: &str,
) -> SecurityCheck {
    SecurityCheck {
        id: id.to_string(),
        status: JsonCheckStatus::from(status),
        observed,
        expected,
        remediation: (status != CheckStatus::Pass).then(|| remediation.to_string()),
    }
}

fn build_security_context(config: &Config) -> SecurityContext {
    SecurityContext {
        container: docker::effective_container_security(&config.docker),
        valid_sensitive_patterns: valid_sensitive_pattern_count(&config.manager.sensitive_patterns),
    }
}

fn build_effective_controls(
    ghost: &config::GhostConfig,
    context: &SecurityContext,
) -> EffectiveControls {
    EffectiveControls {
        container_mode: context.container.container_mode.to_string(),
        caps_dropped: context.container.caps_dropped.clone(),
        rootfs_readonly: context.container.rootfs_readonly,
        network_mode: context.container.network_mode.to_string(),
        pid_limit: context.container.pid_limit,
        memory_limit: context.container.memory_limit,
        cpu_quota: context.container.cpu_quota,
        tool_guard: tools::TOOL_ALLOWLIST_GUARD_ENABLED && !ghost.tools.is_empty(),
        path_guard: tools::PATH_GUARD_ENABLED,
        sensitive_pattern_guard: context.valid_sensitive_patterns > 0,
    }
}

fn container_security_checks(controls: &EffectiveControls) -> Vec<SecurityCheck> {
    vec![
        security_check(
            "container.mode",
            if controls.container_mode == docker::CONTAINER_MODE {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.container_mode.clone(),
            docker::CONTAINER_MODE.to_string(),
            "Route ghost task execution through Docker containers.",
        ),
        security_check(
            "container.caps_dropped",
            if controls
                .caps_dropped
                .iter()
                .any(|cap| cap == docker::CAP_DROP_ALL)
            {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.caps_dropped.join(","),
            format!("includes {}", docker::CAP_DROP_ALL),
            "Drop all container capabilities (`cap_drop = [\"ALL\"]`).",
        ),
        security_check(
            "rootfs.readonly",
            if controls.rootfs_readonly {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.rootfs_readonly.to_string(),
            "true".to_string(),
            "Enable read-only root filesystem for ghost containers.",
        ),
        security_check(
            "network.mode",
            if controls.network_mode == docker::NETWORK_MODE_NONE {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.network_mode.clone(),
            docker::NETWORK_MODE_NONE.to_string(),
            "Disable ghost container networking (`network_mode = \"none\"`).",
        ),
    ]
}

fn limit_security_checks(controls: &EffectiveControls) -> Vec<SecurityCheck> {
    vec![
        security_check(
            "limits.pid",
            if controls.pid_limit > 0 && controls.pid_limit <= docker::DEFAULT_PIDS_LIMIT {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.pid_limit.to_string(),
            format!("set and <= {}", docker::DEFAULT_PIDS_LIMIT),
            "Set a conservative PID limit (default is 256).",
        ),
        security_check(
            "limits.memory",
            if controls.memory_limit > 0 {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.memory_limit.to_string(),
            "positive byte limit".to_string(),
            "Set `[docker].memory_limit` to a positive value.",
        ),
        security_check(
            "limits.cpu",
            if controls.cpu_quota > 0 {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.cpu_quota.to_string(),
            "positive CFS quota".to_string(),
            "Set `[docker].cpu_quota` to a positive value.",
        ),
    ]
}

fn guard_security_checks(controls: &EffectiveControls) -> Vec<SecurityCheck> {
    vec![
        security_check(
            "guard.tool_allowlist",
            if controls.tool_guard {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.tool_guard.to_string(),
            "true".to_string(),
            "Declare an explicit non-empty `ghost.tools` allowlist for each ghost.",
        ),
        security_check(
            "guard.path_validation",
            if controls.path_guard {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.path_guard.to_string(),
            "true".to_string(),
            "Enable path validation in file/path tools to block traversal and sensitive files.",
        ),
        security_check(
            "guard.sensitive_patterns",
            if controls.sensitive_pattern_guard {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            controls.sensitive_pattern_guard.to_string(),
            "true".to_string(),
            "Configure valid `[manager].sensitive_patterns` to gate dangerous shell commands.",
        ),
    ]
}

fn build_security_checks(controls: &EffectiveControls) -> Vec<SecurityCheck> {
    let mut checks = container_security_checks(controls);
    checks.extend(limit_security_checks(controls));
    checks.extend(guard_security_checks(controls));
    checks
}

fn summarize_ghost_security_status(checks: &[SecurityCheck]) -> JsonCheckStatus {
    checks
        .iter()
        .map(|check| check.status.as_check_status())
        .max()
        .unwrap_or(CheckStatus::Pass)
        .into()
}

fn build_security_summary(ghosts: &[GhostSecurityReport]) -> SecuritySummary {
    let checks_pass = ghosts
        .iter()
        .flat_map(|ghost| ghost.checks.iter())
        .filter(|check| check.status == JsonCheckStatus::Pass)
        .count();
    let checks_warn = ghosts
        .iter()
        .flat_map(|ghost| ghost.checks.iter())
        .filter(|check| check.status == JsonCheckStatus::Warn)
        .count();
    let checks_fail = ghosts
        .iter()
        .flat_map(|ghost| ghost.checks.iter())
        .filter(|check| check.status == JsonCheckStatus::Fail)
        .count();
    let ghosts_failing = ghosts
        .iter()
        .filter(|ghost| ghost.status == JsonCheckStatus::Fail)
        .count();

    let overall_status = if checks_fail > 0 {
        JsonCheckStatus::Fail
    } else if checks_warn > 0 {
        JsonCheckStatus::Warn
    } else {
        JsonCheckStatus::Pass
    };

    SecuritySummary {
        overall_status,
        ghosts_total: ghosts.len(),
        ghosts_failing,
        checks_pass,
        checks_warn,
        checks_fail,
    }
}

fn build_security_attestation_report_at(
    config: &Config,
    generated_at: String,
) -> SecurityAttestationReport {
    let context = build_security_context(config);
    let mut ghosts = config
        .ghosts
        .iter()
        .map(|ghost| {
            let effective_controls = build_effective_controls(ghost, &context);
            let checks = build_security_checks(&effective_controls);
            GhostSecurityReport {
                name: ghost.name.clone(),
                status: summarize_ghost_security_status(&checks),
                effective_controls,
                checks,
            }
        })
        .collect::<Vec<_>>();
    ghosts.sort_by(|a, b| a.name.cmp(&b.name));

    SecurityAttestationReport {
        schema_version: SECURITY_ATTESTATION_SCHEMA_VERSION.to_string(),
        generated_at,
        summary: build_security_summary(&ghosts),
        ghosts,
    }
}

fn build_security_attestation_report(config: &Config) -> SecurityAttestationReport {
    build_security_attestation_report_at(
        config,
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    )
}

fn print_security_attestation_report(report: &SecurityAttestationReport) {
    println!("Sparks Security Attestation");
    println!("Schema: {}", report.schema_version);
    println!("Generated: {}", report.generated_at);
    println!(
        "Overall: {} (ghosts={}, failing_ghosts={}, pass={}, warn={}, fail={})",
        report.summary.overall_status.label(),
        report.summary.ghosts_total,
        report.summary.ghosts_failing,
        report.summary.checks_pass,
        report.summary.checks_warn,
        report.summary.checks_fail
    );
    println!();

    for ghost in &report.ghosts {
        println!("Ghost: {} [{}]", ghost.name, ghost.status.label());
        println!(
            "  Effective controls: container_mode={} caps_dropped={} rootfs_readonly={} network_mode={}",
            ghost.effective_controls.container_mode,
            ghost.effective_controls.caps_dropped.join(","),
            ghost.effective_controls.rootfs_readonly,
            ghost.effective_controls.network_mode
        );
        println!(
            "  Limits: pid_limit={} memory_limit={} cpu_quota={}",
            ghost.effective_controls.pid_limit,
            ghost.effective_controls.memory_limit,
            ghost.effective_controls.cpu_quota
        );
        println!(
            "  Guards: tool_allowlist={} path_validation={} sensitive_patterns={}",
            ghost.effective_controls.tool_guard,
            ghost.effective_controls.path_guard,
            ghost.effective_controls.sensitive_pattern_guard
        );
        for check in &ghost.checks {
            println!(
                "  [{}] {} | observed={} | expected={}",
                check.status.label(),
                check.id,
                check.observed,
                check.expected
            );
            if check.status != JsonCheckStatus::Pass {
                if let Some(remediation) = &check.remediation {
                    println!("      remediation: {}", remediation);
                }
            }
        }
        println!();
    }
}

pub async fn run_security_attestation(config: &Config, json: bool) -> anyhow::Result<CheckStatus> {
    let report = build_security_attestation_report(config);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_security_attestation_report(&report);
    }
    Ok(report.summary.overall_status.as_check_status())
}

fn summarize_reports(reports: &[FunnelReport]) -> (CheckStatus, usize, usize, Vec<String>) {
    let fail_count = reports
        .iter()
        .flat_map(|r| r.checks.iter())
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    let warn_count = reports
        .iter()
        .flat_map(|r| r.checks.iter())
        .filter(|c| c.status == CheckStatus::Warn)
        .count();

    let overall = if fail_count > 0 {
        CheckStatus::Fail
    } else if warn_count > 0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Pass
    };

    let mut fixes = Vec::new();
    for report in reports {
        for check in &report.checks {
            if check.status == CheckStatus::Pass {
                continue;
            }
            if let Some(fix) = &check.fix {
                if !fixes.contains(fix) {
                    fixes.push(fix.clone());
                }
            }
        }
    }

    (overall, fail_count, warn_count, fixes)
}

fn print_report(
    db_path: &std::path::Path,
    reports: &[FunnelReport],
    overall: CheckStatus,
    fail_count: usize,
    warn_count: usize,
    fixes: &[String],
) {
    println!("Sparks Funnel Health");
    println!("Database: {}", db_path.display());
    println!(
        "Overall: {} (fails={}, warns={})",
        overall.label(),
        fail_count,
        warn_count
    );
    println!();

    for report in reports {
        println!("{} [{}]", report.name, report.worst_status().label());
        if let Some(bp) = report.break_point() {
            println!("  Break point: {}", bp.stage);
        }
        for check in &report.checks {
            println!(
                "  [{}] {}: {}",
                check.status.label(),
                check.stage,
                check.detail
            );
        }
        println!();
    }

    if !fixes.is_empty() {
        println!("Recommended Fixes:");
        for (idx, fix) in fixes.iter().enumerate() {
            println!("{}. {}", idx + 1, fix);
        }
    }
}

async fn collect_funnel_inputs(
    config: &Config,
    skip_llm: bool,
) -> anyhow::Result<(DoctorSnapshot, LlmHealth)> {
    let snap = collect_snapshot(config)?;
    let llm = evaluate_llm_health(config, skip_llm).await;
    Ok((snap, llm))
}

async fn build_funnel_reports(
    config: &Config,
    snap: &DoctorSnapshot,
    llm: &LlmHealth,
    skip_llm: bool,
) -> Vec<FunnelReport> {
    let mut reports = vec![
        build_funnel1(config, snap, llm),
        build_funnel2(config, snap, llm),
        build_funnel3(config, snap, llm),
        build_funnel4(config, snap, llm).await,
        build_local_only_funnel(config, skip_llm).await,
    ];
    reports.push(build_self_dev_runtime_funnel(config, snap).await);
    reports
}

fn render_funnel_report(
    snap: &DoctorSnapshot,
    reports: &[FunnelReport],
    overall: CheckStatus,
    fail_count: usize,
    warn_count: usize,
    fixes: &[String],
) {
    print_report(
        &snap.db_path,
        reports,
        overall,
        fail_count,
        warn_count,
        fixes,
    );
}

pub async fn run_funnel_health(config: &Config, skip_llm: bool) -> anyhow::Result<CheckStatus> {
    let (snap, llm) = collect_funnel_inputs(config, skip_llm).await?;
    let reports = build_funnel_reports(config, &snap, &llm, skip_llm).await;
    let (overall, fail_count, warn_count, fixes) = summarize_reports(&reports);
    render_funnel_report(&snap, &reports, overall, fail_count, warn_count, &fixes);
    Ok(overall)
}

/// Validate tool profile references and tool registration.
/// Returns a list of warning strings (not errors — startup continues).
pub fn validate_tool_profiles(
    profiles: &crate::config::ToolProfiles,
    referenced_profiles: &[String],
    registered_tools: &[String],
) -> Vec<String> {
    let registered: std::collections::HashSet<_> = registered_tools.iter().collect();
    let mut issues = Vec::new();

    for profile_ref in referenced_profiles {
        if !profiles.contains_key(profile_ref) {
            issues.push(format!(
                "Ghost references unknown tool profile '{}' — check [tool_profiles] in config",
                profile_ref
            ));
        }
    }

    for (profile_name, tools) in profiles {
        for tool in tools {
            if !registered.contains(tool) {
                issues.push(format!(
                    "Tool profile '{}' references unregistered tool '{}' — may be an MCP tool not yet connected",
                    profile_name, tool
                ));
            }
        }
    }

    issues
}

#[cfg(test)]
mod profile_validation_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validate_profiles_finds_missing_profile_reference() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("researcher".to_string(), vec!["web_search".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["nonexistent_profile".to_string()], // ghost references this
            &["web_search".to_string()],           // registered tools
        );
        assert!(issues.iter().any(|i| i.contains("nonexistent_profile")));
    }

    #[test]
    fn validate_profiles_finds_unregistered_tool_in_profile() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("test".to_string(), vec!["phantom_tool".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["test".to_string()],
            &["web_search".to_string()], // phantom_tool not registered
        );
        assert!(issues.iter().any(|i| i.contains("phantom_tool")));
    }

    #[test]
    fn validate_profiles_no_issues_when_valid() {
        let mut profiles: crate::config::ToolProfiles = HashMap::new();
        profiles.insert("researcher".to_string(), vec!["web_search".to_string()]);

        let issues = validate_tool_profiles(
            &profiles,
            &["researcher".to_string()],
            &["web_search".to_string()],
        );
        assert!(issues.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_attestation_json_schema_v1_shape() {
        let config = Config::default();
        let report =
            build_security_attestation_report_at(&config, "2026-03-03T00:00:00.000Z".to_string());
        let value = match serde_json::to_value(&report) {
            Ok(v) => v,
            Err(e) => panic!("failed to serialize security report: {}", e),
        };
        let root = match value.as_object() {
            Some(v) => v,
            None => panic!("security report root should be an object"),
        };

        assert_eq!(
            root.get("schema_version")
                .and_then(serde_json::Value::as_str),
            Some("v1")
        );
        assert!(root.contains_key("generated_at"));
        assert!(root.contains_key("summary"));
        assert!(root.contains_key("ghosts"));

        let ghosts = match root.get("ghosts").and_then(serde_json::Value::as_array) {
            Some(v) => v,
            None => panic!("ghosts should be an array"),
        };
        assert!(!ghosts.is_empty());

        for ghost in ghosts {
            let status = ghost.get("status").and_then(serde_json::Value::as_str);
            assert!(matches!(status, Some("pass" | "warn" | "fail")));
            let checks = match ghost.get("checks").and_then(serde_json::Value::as_array) {
                Some(v) => v,
                None => panic!("checks should be an array"),
            };
            assert!(!checks.is_empty());
            for check in checks {
                let check_status = check.get("status").and_then(serde_json::Value::as_str);
                assert!(matches!(check_status, Some("pass" | "warn" | "fail")));
                assert!(check.get("id").is_some());
                assert!(check.get("observed").is_some());
                assert!(check.get("expected").is_some());
                assert!(check.get("remediation").is_some());
            }
        }
    }

    #[test]
    fn security_attestation_fails_without_valid_sensitive_patterns() {
        let mut config = Config::default();
        config.manager.sensitive_patterns = vec!["[".to_string()];

        let report =
            build_security_attestation_report_at(&config, "2026-03-03T00:00:00.000Z".to_string());
        assert_eq!(report.summary.overall_status, JsonCheckStatus::Fail);

        for ghost in &report.ghosts {
            let check = match ghost
                .checks
                .iter()
                .find(|check| check.id == "guard.sensitive_patterns")
            {
                Some(v) => v,
                None => panic!("guard.sensitive_patterns check missing for {}", ghost.name),
            };
            assert_eq!(check.status, JsonCheckStatus::Fail);
            assert!(check.remediation.is_some());
        }
    }

    #[test]
    fn security_attestation_uses_effective_limit_overrides() {
        let mut config = Config::default();
        config.docker.memory_limit = 123_456_789;
        config.docker.cpu_quota = 65_000;

        let report =
            build_security_attestation_report_at(&config, "2026-03-03T00:00:00.000Z".to_string());
        for ghost in &report.ghosts {
            assert_eq!(ghost.effective_controls.memory_limit, 123_456_789);
            assert_eq!(ghost.effective_controls.cpu_quota, 65_000);
        }
    }

    #[test]
    fn parse_compile_runtime_probe_reads_capability_markers() {
        let out = "\
__SPARKS_CARGO_VERSION__:cargo 1.88.0\n\
__SPARKS_RG__:0\n\
__SPARKS_WRITE_FAIL__:/tmp/cargo-home\n\
__SPARKS_MISSING_BIN__:cargo\n";
        let probe = parse_compile_runtime_probe(out);
        assert_eq!(probe.cargo_version.as_deref(), Some("cargo 1.88.0"));
        assert!(!probe.rg_available);
        assert_eq!(probe.write_fail_paths, vec!["/tmp/cargo-home".to_string()]);
        assert_eq!(probe.missing_bins, vec!["cargo".to_string()]);
    }

    #[test]
    fn evaluate_compile_runtime_probe_allows_missing_rg_with_fallback() {
        let probe = CompileRuntimeProbe {
            missing_bins: vec![],
            write_fail_paths: vec![],
            cargo_version: Some("cargo 1.88.0".to_string()),
            rg_available: false,
        };
        let result = evaluate_compile_runtime_probe(probe);
        assert!(result.is_ok());
        assert!(!result.rg_available);
        assert!(result.detail.contains("fallback enabled"));
    }

    #[test]
    fn evaluate_compile_runtime_probe_flags_missing_required_tools() {
        let probe = CompileRuntimeProbe {
            missing_bins: vec!["cargo".to_string()],
            write_fail_paths: vec![],
            cargo_version: None,
            rg_available: true,
        };
        let result = evaluate_compile_runtime_probe(probe);
        assert_eq!(
            result.reason_code,
            Some(reason_codes::REASON_GHOST_TOOL_UNAVAILABLE)
        );
    }

    #[test]
    fn evaluate_compile_runtime_probe_flags_unwritable_temp_dirs() {
        let probe = CompileRuntimeProbe {
            missing_bins: vec![],
            write_fail_paths: vec!["/tmp/rustup-home/tmp".to_string()],
            cargo_version: Some("cargo 1.88.0".to_string()),
            rg_available: true,
        };
        let result = evaluate_compile_runtime_probe(probe);
        assert_eq!(
            result.reason_code,
            Some(reason_codes::REASON_GHOST_RUST_TEMP_UNWRITABLE)
        );
    }

    #[test]
    fn ghost_preflight_failure_message_is_reason_tagged() {
        let preflight = GhostCompilePreflight {
            reason_code: Some(reason_codes::REASON_GHOST_RUNTIME_CAPABILITY_MISMATCH),
            detail: "failed to start ghost container".to_string(),
            remediation: Some("fix image".to_string()),
            rg_available: false,
        };
        let msg = preflight.failure_message("coder").unwrap_or_default();
        assert!(msg.contains("[reason:ghost_runtime_capability_mismatch]"));
        assert!(msg.contains("Remediation: fix image"));
    }

    #[test]
    fn local_only_provider_requires_ollama() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::LocalOnly;
        config.llm.provider = "openai".to_string();

        let check = local_only_provider_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn local_only_ollama_endpoint_must_be_loopback() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::LocalOnly;
        config.llm.provider = "ollama".to_string();
        config.ollama.url = "http://example.com:11434".to_string();

        let check = local_only_ollama_endpoint_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn local_only_flags_outbound_integrations() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::LocalOnly;
        config.langfuse.enabled = true;

        let check = local_only_outbound_integrations_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("langfuse"));
    }

    #[test]
    fn local_only_storage_paths_reject_urls() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::LocalOnly;
        config.db.path = "https://example.com/sparks.db".to_string();

        let check = local_only_storage_paths_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn local_only_checks_are_informational_when_profile_not_selected() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::Standard;
        config.llm.provider = "openai".to_string();

        let check = local_only_provider_check(&config);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("runtime.profile != local_only"));
    }

    #[test]
    fn self_dev_trusted_requires_self_dev_enabled() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::SelfDevTrusted;
        config.self_dev.enabled = false;

        let check = self_dev_trusted_enablement_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn self_dev_trusted_requires_repo_allowlist() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::SelfDevTrusted;
        config.self_dev.enabled = true;
        config.self_dev.trusted_repos.clear();

        let check = self_dev_trusted_repo_allowlist_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("self_dev_mode_restriction"));
    }

    #[test]
    fn self_dev_trusted_budget_check_enforces_minimums() {
        let mut config = Config::default();
        config.runtime.profile = config::RuntimeProfile::SelfDevTrusted;
        config.self_dev.enabled = true;
        config.self_dev.trusted_repos = vec!["sparks".to_string()];
        config.docker.timeout_secs = 120;
        config.manager.max_steps = 10;

        let check = self_dev_trusted_budget_check(&config);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("docker.timeout_secs=120 (min 600)"));
    }
}
