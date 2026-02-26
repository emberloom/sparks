use std::path::PathBuf;

use rusqlite::Connection;

use crate::config::{self, Config};
use crate::docker;
use crate::knobs;
use crate::profiles;
use crate::tool_usage::ToolUsageStore;

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
    CheckItem {
        stage: "Credentials storage hygiene",
        status: if snap.inline_secret_labels.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: if snap.inline_secret_labels.is_empty() {
            "No inline credentials found in config".to_string()
        } else {
            format!(
                "Inline secrets configured for: {}",
                snap.inline_secret_labels.join(", ")
            )
        },
        fix: (!snap.inline_secret_labels.is_empty()).then(|| {
            "Move secrets out of config.toml into env vars or a .env file (gitignored)."
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

    let mut failures = Vec::new();
    for provider_name in config.provider_candidates() {
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
            "Run Athena with self-dev enabled long enough for the code indexer interval to elapse."
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

    let session = match docker::DockerSession::new(coder, &config.docker).await {
        Ok(s) => s,
        Err(e) => {
            return CheckItem {
                stage: "Rust toolchain in execution env",
                status: CheckStatus::Fail,
                detail: format!("failed to start coder container: {}", e),
                fix: Some(
                    "Ensure Docker daemon is running and configured image exists locally/pullable."
                        .to_string(),
                ),
            };
        }
    };

    let probe = session
        .exec("if command -v cargo >/dev/null 2>&1; then cargo --version; else echo __ATHENA_CARGO_MISSING__; fi")
        .await;
    let _ = session.close().await;

    match probe {
        Ok(output) if !output.contains("__ATHENA_CARGO_MISSING__") => CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Pass,
            detail: output.trim().to_string(),
            fix: None,
        },
        Ok(_) => CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Fail,
            detail: "cargo is missing inside coder container".to_string(),
            fix: Some(
                "Set `[docker].image` or `coder.image` to a Rust image (e.g. `rust:1.84-slim`) and ensure PATH includes `/usr/local/cargo/bin`.".to_string(),
            ),
        },
        Err(e) => CheckItem {
            stage: "Rust toolchain in execution env",
            status: CheckStatus::Fail,
            detail: format!("cargo probe failed: {}", e),
            fix: Some("Fix container startup/image issues, then re-run `athena doctor`.".to_string()),
        },
    }
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
    println!("Athena Funnel Health");
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

pub async fn run_funnel_health(config: &Config, skip_llm: bool) -> anyhow::Result<CheckStatus> {
    let snap = collect_snapshot(config)?;
    let llm = evaluate_llm_health(config, skip_llm).await;

    let reports = vec![
        build_funnel1(config, &snap, &llm),
        build_funnel2(config, &snap, &llm),
        build_funnel3(config, &snap, &llm),
        build_funnel4(config, &snap, &llm).await,
    ];

    let (overall, fail_count, warn_count, fixes) = summarize_reports(&reports);
    print_report(
        &snap.db_path,
        &reports,
        overall,
        fail_count,
        warn_count,
        &fixes,
    );
    Ok(overall)
}
