#![allow(dead_code)]

mod config;
mod confirm;
mod core;
mod db;
mod docker;
mod doctor;
mod dynamic_tools;
mod embeddings;
mod error;
mod executor;
mod feature_contract;
mod heartbeat;
mod introspect;
mod knobs;
mod kpi;
mod langfuse;
mod llm;
mod manager;
mod memory;
mod mood;
mod observer;
mod proactive;
mod profiles;
mod pulse;
mod randomness;
mod scheduler;
mod self_heal;
mod strategy;
#[cfg(feature = "telegram")]
mod telegram;
mod tool_usage;
mod tools;

use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use confirm::CliConfirmer;
use core::{AthenaCore, CoreEvent, SessionContext};
use embeddings::Embedder;
use memory::MemoryStore;
use observer::ObserverCategory;
use scheduler::Schedule;

const OUTCOME_REASON_DISPATCH_TIMEOUT: &str = "dispatch_timeout";
const OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED: &str = "dispatch_channel_closed";
const OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT: &str = "outcome_wait_timeout";

#[derive(Parser)]
#[command(name = "athena", about = "Secure autonomous multi-agent system")]
struct Cli {
    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Auto-approve all tool executions (skip confirmation prompts)
    #[arg(short = 'y', long = "yes")]
    auto_approve: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive chat (default)
    Chat,
    /// Manage long-term memory
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// List configured ghosts
    Ghosts,
    /// Run as a Telegram bot (requires --features telegram)
    #[cfg(feature = "telegram")]
    Telegram,
    /// Watch internal observer events in real time
    Observe,
    /// Manage scheduled jobs
    Jobs {
        #[command(subcommand)]
        action: JobsAction,
    },
    /// Dispatch one autonomous task from CLI and wait for its pulse result
    Dispatch {
        /// Goal to execute
        #[arg(long)]
        goal: String,
        /// Optional context for the ghost
        #[arg(long)]
        context: Option<String>,
        /// Optional ghost name (e.g., coder, scout). If omitted, orchestrator classifies.
        #[arg(long)]
        ghost: Option<String>,
        /// Optional memory auto-store category (adds [auto_store:<category>] context tag)
        #[arg(long)]
        auto_store: Option<String>,
        /// Optional CLI tool override for this dispatch run
        #[arg(long, value_parser = ["claude_code", "codex", "opencode"])]
        cli_tool: Option<String>,
        /// Optional coding model override for this dispatch run
        #[arg(long)]
        cli_model: Option<String>,
        /// How long to wait for an autonomous pulse result
        #[arg(long, default_value_t = 120)]
        wait_secs: u64,
        /// Mission lane for KPI attribution
        #[arg(long, default_value = "delivery")]
        lane: String,
        /// Risk tier for KPI attribution
        #[arg(long, default_value = "medium")]
        risk: String,
        /// Repo/product label for KPI attribution
        #[arg(long)]
        repo: Option<String>,
    },
    /// Run end-to-end diagnostics for all self-improvement funnels
    Doctor {
        /// Skip live LLM connectivity checks (useful for CI/offline checks)
        #[arg(long)]
        skip_llm: bool,
        /// Exit non-zero when overall status is FAIL
        #[arg(long)]
        ci: bool,
        /// Exit non-zero on WARN as well (implies stricter CI gate)
        #[arg(long)]
        fail_on_warn: bool,
    },
    /// Mission KPI tracking (status, snapshot, history)
    Kpi {
        #[command(subcommand)]
        action: KpiAction,
    },
    /// Execute multi-task feature contracts with DAG dependency ordering
    Feature {
        #[command(subcommand)]
        action: FeatureAction,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// List all active memories
    List,
    /// Add a memory
    Add {
        /// Category (e.g., "lesson", "fact", "preference")
        category: String,
        /// Content
        content: String,
    },
    /// Retire a memory by ID
    Retire {
        /// Memory ID
        id: String,
    },
}

#[derive(Subcommand)]
enum JobsAction {
    /// List all scheduled jobs
    List,
    /// Add a new job
    Add {
        /// Job name
        #[arg(long)]
        name: String,
        /// Interval in seconds (for interval jobs)
        #[arg(long)]
        every: Option<u64>,
        /// Cron expression (e.g., "0 0 9 * * MON-FRI *")
        #[arg(long)]
        cron: Option<String>,
        /// Prompt to send to LLM when the job fires
        #[arg(long)]
        prompt: String,
    },
    /// Delete a job by ID
    Delete {
        /// Job ID (prefix match)
        id: String,
    },
}

#[derive(Subcommand)]
enum KpiAction {
    /// Compute and print KPI status for current state
    Status {
        /// Mission lane: delivery | self_improvement
        #[arg(long, default_value = "self_improvement")]
        lane: String,
        /// Product/repo label
        #[arg(long)]
        repo: Option<String>,
        /// Risk tier: low | medium | high
        #[arg(long, default_value = "medium")]
        risk: String,
    },
    /// Compute, persist, and optionally export a KPI snapshot
    Snapshot {
        /// Mission lane: delivery | self_improvement
        #[arg(long, default_value = "self_improvement")]
        lane: String,
        /// Product/repo label
        #[arg(long)]
        repo: Option<String>,
        /// Risk tier: low | medium | high
        #[arg(long, default_value = "medium")]
        risk: String,
        /// Export snapshot to Langfuse as trace event
        #[arg(long)]
        langfuse: bool,
    },
    /// Show stored KPI snapshot history
    History {
        /// Optional lane filter
        #[arg(long)]
        lane: Option<String>,
        /// Optional repo filter
        #[arg(long)]
        repo: Option<String>,
        /// Max rows
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum FeatureAction {
    /// Validate a feature contract file (YAML or JSON)
    Validate {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
    },
    /// Print execution batches and topological order from a feature contract
    Plan {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
    },
    /// Run feature-level verification checks mapped to acceptance criteria
    Verify {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
    },
    /// Dispatch feature tasks using DAG order and wait for terminal outcomes
    Dispatch {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
        /// Global wait timeout per task (seconds) when task-level wait_secs is unset
        #[arg(long, default_value_t = 180)]
        wait_secs: u64,
        /// Continue dispatching independent tasks after failures
        #[arg(long)]
        continue_on_failure: bool,
        /// Resolve DAG and print execution plan without dispatching tasks
        #[arg(long)]
        dry_run: bool,
        /// Optional CLI tool override for this run
        #[arg(long, value_parser = ["claude_code", "codex", "opencode"])]
        cli_tool: Option<String>,
        /// Optional coding model override for this run
        #[arg(long)]
        cli_model: Option<String>,
        /// Override lane for all tasks
        #[arg(long)]
        lane: Option<String>,
        /// Override risk tier for all tasks
        #[arg(long)]
        risk: Option<String>,
        /// Override repo/product label for all tasks
        #[arg(long)]
        repo: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "athena=info".parse().unwrap()),
        )
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .compact()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Handle observe subcommand early — it doesn't need config/db/LLM
    if matches!(cli.command, Some(Commands::Observe)) {
        return run_observe().await;
    }

    let auto_approve = cli.auto_approve;
    let config = Config::load(cli.config.as_deref())?;

    // Initialize database
    let db_path = config.db_path()?;
    let conn = db::init_db(&db_path)?;
    let memory = Arc::new(MemoryStore::new(
        conn,
        config.memory.recency_half_life_days,
        config.memory.dedup_threshold,
    ));

    let needs_cli_embedder = matches!(cli.command, Some(Commands::Memory { .. }));

    // Initialize embedder for CLI paths that need it.
    let embedder = if needs_cli_embedder && config.embedding.enabled {
        config.resolve_model_dir().ok().and_then(|dir| {
            Embedder::ensure_model(&dir).ok()?;
            match Embedder::new(&dir) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!("Embedder unavailable for CLI: {}", e);
                    None
                }
            }
        })
    } else {
        None
    };

    // Backfill any memories missing embeddings (fast no-op when none exist).
    if needs_cli_embedder {
        if let Some(ref e) = embedder {
            core::backfill_embeddings(&memory, e);
        }
    }

    match cli.command {
        Some(Commands::Memory { action }) => handle_memory(action, &memory, embedder.as_ref())?,
        Some(Commands::Ghosts) => {
            // Start core to get merged ghost list (config + profiles)
            let handle = AthenaCore::start(config, memory).await?;
            for g in handle.list_ghosts() {
                println!("  {} — {} [{}]", g.name, g.description, g.tools.join(", "));
            }
        }
        #[cfg(feature = "telegram")]
        Some(Commands::Telegram) => {
            let system_info = telegram::SystemInfo {
                provider: config.llm.provider.clone(),
                model: match config.llm.provider.as_str() {
                    "openrouter" => config
                        .openrouter
                        .as_ref()
                        .map(|c| c.model.clone())
                        .unwrap_or_default(),
                    "zen" => config
                        .zen
                        .as_ref()
                        .map(|c| c.model.clone())
                        .unwrap_or_default(),
                    _ => config.ollama.model.clone(),
                },
                temperature: match config.llm.provider.as_str() {
                    "openrouter" => config
                        .openrouter
                        .as_ref()
                        .map(|c| c.temperature)
                        .unwrap_or(0.3),
                    "zen" => config.zen.as_ref().map(|c| c.temperature).unwrap_or(0.3),
                    _ => config.ollama.temperature,
                },
                max_tokens: match config.llm.provider.as_str() {
                    "openrouter" => config
                        .openrouter
                        .as_ref()
                        .map(|c| c.max_tokens)
                        .unwrap_or(4096),
                    "zen" => config.zen.as_ref().map(|c| c.max_tokens).unwrap_or(4096),
                    _ => config.ollama.max_tokens,
                },
                started_at: tokio::time::Instant::now(),
            };
            let handle = AthenaCore::start(config.clone(), memory).await?;
            telegram::run_telegram(handle, config.telegram, system_info).await?;
        }
        Some(Commands::Observe) => unreachable!(), // handled above
        Some(Commands::Jobs { action }) => {
            let handle = AthenaCore::start(config, memory).await?;
            handle_jobs(action, &handle)?;
        }
        Some(Commands::Dispatch {
            goal,
            context,
            ghost,
            auto_store,
            cli_tool,
            cli_model,
            wait_secs,
            lane,
            risk,
            repo,
        }) => {
            run_dispatch(
                config, memory, goal, context, ghost, auto_store, cli_tool, cli_model, wait_secs,
                lane, risk, repo,
            )
            .await?
        }
        Some(Commands::Doctor {
            skip_llm,
            ci,
            fail_on_warn,
        }) => {
            let overall = doctor::run_funnel_health(&config, skip_llm).await?;
            if ci {
                if overall == doctor::CheckStatus::Fail
                    || (fail_on_warn && overall == doctor::CheckStatus::Warn)
                {
                    anyhow::bail!("doctor status: {}", overall.label());
                }
            }
        }
        Some(Commands::Kpi { action }) => handle_kpi(action, &config).await?,
        Some(Commands::Feature { action }) => handle_feature(action, config, memory).await?,
        Some(Commands::Chat) | None => run_chat(config, memory, auto_approve).await?,
    }

    Ok(())
}

fn validate_lane(lane: &str) -> anyhow::Result<()> {
    match lane {
        "delivery" | "self_improvement" => Ok(()),
        _ => anyhow::bail!("Invalid lane '{}'. Use: delivery | self_improvement", lane),
    }
}

fn validate_risk(risk: &str) -> anyhow::Result<()> {
    match risk {
        "low" | "medium" | "high" => Ok(()),
        _ => anyhow::bail!("Invalid risk '{}'. Use: low | medium | high", risk),
    }
}

async fn handle_kpi(action: KpiAction, config: &Config) -> anyhow::Result<()> {
    let conn = kpi::open_connection(config)?;
    match action {
        KpiAction::Status { lane, repo, risk } => {
            validate_lane(&lane)?;
            validate_risk(&risk)?;
            let repo = repo.unwrap_or_else(kpi::default_repo_name);
            let snapshot = kpi::compute_snapshot(&conn, &lane, &repo, &risk)?;
            kpi::print_snapshot(&snapshot);
        }
        KpiAction::Snapshot {
            lane,
            repo,
            risk,
            langfuse,
        } => {
            validate_lane(&lane)?;
            validate_risk(&risk)?;
            let repo = repo.unwrap_or_else(kpi::default_repo_name);
            let snapshot = kpi::compute_snapshot(&conn, &lane, &repo, &risk)?;
            kpi::store_snapshot(&conn, &snapshot)?;
            kpi::print_snapshot(&snapshot);
            println!("snapshot_saved=true");
            if langfuse {
                match kpi::emit_snapshot_to_langfuse(config, &snapshot).await {
                    Ok(_) => println!("langfuse_export=ok"),
                    Err(e) => println!("langfuse_export=failed ({})", e),
                }
            }
        }
        KpiAction::History { lane, repo, limit } => {
            let rows = kpi::list_history(&conn, lane.as_deref(), repo.as_deref(), limit)?;
            kpi::print_history(&rows);
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum FeatureRunStatus {
    Succeeded,
    Failed(String),
    Skipped(String),
}

impl FeatureRunStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Skipped(_) => "skipped",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed(r) | Self::Skipped(r) => Some(r),
            Self::Succeeded => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FeatureTaskLedgerRow {
    task_id: String,
    dispatch_task_id: Option<String>,
    status: String,
    reason: Option<String>,
    mapped_acceptance: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureAcceptanceLedgerRow {
    acceptance_id: String,
    covered_by_tasks: Vec<String>,
    succeeded_tasks: Vec<String>,
    covered: bool,
    satisfied: bool,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureRunLedger {
    timestamp_utc: String,
    feature_id: String,
    contract_path: String,
    tasks: Vec<FeatureTaskLedgerRow>,
    acceptance: Vec<FeatureAcceptanceLedgerRow>,
    summary: FeatureRunSummary,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureRunSummary {
    succeeded: usize,
    failed: usize,
    skipped: usize,
    acceptance_covered: bool,
    acceptance_satisfied: bool,
    promotable: bool,
    promotion_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureVerifyCheckRow {
    check_id: String,
    command: String,
    required: bool,
    status: String,
    exit_code: Option<i32>,
    mapped_acceptance: Vec<String>,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureVerifyAcceptanceRow {
    acceptance_id: String,
    checks: Vec<String>,
    passed_checks: Vec<String>,
    satisfied: bool,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureVerifyLedger {
    timestamp_utc: String,
    feature_id: String,
    contract_path: String,
    checks: Vec<FeatureVerifyCheckRow>,
    acceptance: Vec<FeatureVerifyAcceptanceRow>,
    summary: FeatureVerifySummary,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureVerifySummary {
    checks_total: usize,
    checks_passed: usize,
    checks_failed: usize,
    required_checks_failed: usize,
    acceptance_satisfied: bool,
    promotable: bool,
    promotion_reasons: Vec<String>,
}

async fn handle_feature(
    action: FeatureAction,
    config: Config,
    memory: Arc<MemoryStore>,
) -> anyhow::Result<()> {
    match action {
        FeatureAction::Validate { file } => {
            let contract = feature_contract::load_feature_contract(&file)?;
            let batches = contract.execution_batches()?;
            let enabled = contract.tasks.iter().filter(|t| t.enabled).count();
            let acceptance_count = contract.acceptance_criteria.len();
            let checks_count = contract.verification_checks.len();
            println!(
                "feature_id={} valid=true tasks_enabled={} acceptance={} checks={} batches={}",
                contract.feature_id,
                enabled,
                acceptance_count,
                checks_count,
                batches.len()
            );
        }
        FeatureAction::Plan { file } => {
            let contract = feature_contract::load_feature_contract(&file)?;
            print_feature_plan(&contract)?;
        }
        FeatureAction::Verify { file } => {
            let contract = feature_contract::load_feature_contract(&file)?;
            let ledger = run_feature_verify(&contract, &file)?;
            let (json_path, md_path) = write_feature_verify_artifacts(&ledger)?;
            println!("feature_verify_json={}", json_path.display());
            println!("feature_verify_md={}", md_path.display());
            println!(
                "feature_id={} verify checks_total={} checks_failed={} required_checks_failed={} acceptance_satisfied={} promotable={}",
                contract.feature_id,
                ledger.summary.checks_total,
                ledger.summary.checks_failed,
                ledger.summary.required_checks_failed,
                ledger.summary.acceptance_satisfied,
                ledger.summary.promotable
            );
            if !ledger.summary.promotable {
                if !ledger.summary.promotion_reasons.is_empty() {
                    println!(
                        "feature_verify_reasons={}",
                        ledger.summary.promotion_reasons.join(" | ")
                    );
                }
                anyhow::bail!("Feature verify failed promotion gate.");
            }
        }
        FeatureAction::Dispatch {
            file,
            wait_secs,
            continue_on_failure,
            dry_run,
            cli_tool,
            cli_model,
            lane,
            risk,
            repo,
        } => {
            let contract = feature_contract::load_feature_contract(&file)?;
            let batches = contract.execution_batches()?;
            if batches.is_empty() {
                println!("feature_id={} nothing_to_run=true", contract.feature_id);
                return Ok(());
            }
            if dry_run {
                print_feature_plan(&contract)?;
                println!("feature_id={} dry_run=true", contract.feature_id);
                return Ok(());
            }

            if let Some(l) = lane.as_deref() {
                validate_lane(l)?;
            }
            if let Some(r) = risk.as_deref() {
                validate_risk(r)?;
            }

            let handle = AthenaCore::start(config.clone(), memory).await?;
            if cli_tool.is_some() || cli_model.is_some() {
                let mut knobs = handle
                    .knobs
                    .write()
                    .map_err(|_| anyhow::anyhow!("Failed to lock runtime knobs"))?;
                if let Some(tool) = cli_tool.as_deref() {
                    knobs.set("cli_tool", tool).map_err(anyhow::Error::msg)?;
                    eprintln!("Feature override: cli_tool={}", tool);
                }
                if let Some(model) = cli_model.as_deref() {
                    knobs.set("cli_model", model).map_err(anyhow::Error::msg)?;
                    eprintln!("Feature override: cli_model={}", model);
                }
            }

            println!(
                "feature_id={} mode=dispatch batches={} continue_on_failure={}",
                contract.feature_id,
                batches.len(),
                continue_on_failure
            );
            let mut statuses: std::collections::HashMap<String, FeatureRunStatus> =
                std::collections::HashMap::new();
            let mut dispatch_ids: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let default_lane = lane
                .or_else(|| contract.lane.clone())
                .unwrap_or_else(|| "delivery".to_string());
            let default_risk = risk
                .or_else(|| contract.risk.clone())
                .unwrap_or_else(|| "medium".to_string());
            let default_repo = repo
                .or_else(|| contract.repo.clone())
                .unwrap_or_else(kpi::default_repo_name);
            validate_lane(&default_lane)?;
            validate_risk(&default_risk)?;
            let mut aborted_on_failure = false;

            for (idx, batch) in batches.iter().enumerate() {
                println!("batch={} tasks={}", idx + 1, batch.join(","));
                for task_id in batch {
                    let task = contract.task_by_id(task_id).ok_or_else(|| {
                        anyhow::anyhow!("Task '{}' missing from contract", task_id)
                    })?;
                    if let Some(reason) = blocked_dependency_reason(task, &statuses) {
                        println!(
                            "task={} result=skipped reason={}",
                            task.id,
                            reason.replace('\n', " ")
                        );
                        statuses.insert(task.id.clone(), FeatureRunStatus::Skipped(reason));
                        continue;
                    }

                    let lane = task.lane.clone().unwrap_or_else(|| default_lane.clone());
                    let risk = task.risk.clone().unwrap_or_else(|| default_risk.clone());
                    let repo = task.repo.clone().unwrap_or_else(|| default_repo.clone());
                    validate_lane(&lane)?;
                    validate_risk(&risk)?;

                    let task_wait = task.wait_secs.unwrap_or(wait_secs);
                    let task_context = build_feature_task_context(&contract, task);
                    let mut pulse_rx = handle.pulse_bus.subscribe();
                    let task_dispatch_id = handle
                        .dispatch_task(core::AutonomousTask {
                            goal: task.goal.clone(),
                            context: task_context,
                            ghost: task.ghost.clone(),
                            target: crate::pulse::PulseTarget::Broadcast,
                            lane,
                            risk_tier: risk,
                            repo,
                            task_id: None,
                        })
                        .await?;

                    println!(
                        "task={} dispatched_task_id={} wait_secs={}",
                        task.id, task_dispatch_id, task_wait
                    );
                    dispatch_ids.insert(task.id.clone(), task_dispatch_id.clone());
                    let wait =
                        wait_for_autonomous_pulse(&mut pulse_rx, &task_dispatch_id, task_wait)
                            .await;
                    let run_status = match wait {
                        WaitForAutonomousOutcome::Received => {
                            match wait_for_terminal_outcome_status(&config, &task_dispatch_id, 10)
                                .await?
                            {
                                Some(status) if status == "succeeded" => {
                                    FeatureRunStatus::Succeeded
                                }
                                Some(status) => {
                                    FeatureRunStatus::Failed(format!("terminal_status={}", status))
                                }
                                None => {
                                    mark_dispatch_task_failed_if_started(
                                        &config,
                                        &task_dispatch_id,
                                        OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT,
                                    );
                                    FeatureRunStatus::Failed(
                                        OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT.to_string(),
                                    )
                                }
                            }
                        }
                        WaitForAutonomousOutcome::TimedOut => {
                            mark_dispatch_task_failed_if_started(
                                &config,
                                &task_dispatch_id,
                                OUTCOME_REASON_DISPATCH_TIMEOUT,
                            );
                            FeatureRunStatus::Failed(OUTCOME_REASON_DISPATCH_TIMEOUT.to_string())
                        }
                        WaitForAutonomousOutcome::ChannelClosed => {
                            mark_dispatch_task_failed_if_started(
                                &config,
                                &task_dispatch_id,
                                OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED,
                            );
                            FeatureRunStatus::Failed(
                                OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED.to_string(),
                            )
                        }
                    };
                    match &run_status {
                        FeatureRunStatus::Succeeded => {
                            println!("task={} result=succeeded", task.id)
                        }
                        FeatureRunStatus::Failed(reason) => println!(
                            "task={} result=failed reason={}",
                            task.id,
                            reason.replace('\n', " ")
                        ),
                        FeatureRunStatus::Skipped(reason) => println!(
                            "task={} result=skipped reason={}",
                            task.id,
                            reason.replace('\n', " ")
                        ),
                    }
                    statuses.insert(task.id.clone(), run_status.clone());
                    if matches!(run_status, FeatureRunStatus::Failed(_)) && !continue_on_failure {
                        aborted_on_failure = true;
                        break;
                    }
                }
                if aborted_on_failure {
                    break;
                }
            }

            let mut succeeded = 0usize;
            let mut failed = 0usize;
            let mut skipped = 0usize;
            for status in statuses.values() {
                match status {
                    FeatureRunStatus::Succeeded => succeeded += 1,
                    FeatureRunStatus::Failed(_) => failed += 1,
                    FeatureRunStatus::Skipped(_) => skipped += 1,
                }
            }
            let ledger = build_feature_run_ledger(
                &contract,
                &file,
                &statuses,
                &dispatch_ids,
                succeeded,
                failed,
                skipped,
            );
            let (ledger_json, ledger_md) = write_feature_ledger_artifacts(&ledger)?;
            println!("feature_ledger_json={}", ledger_json.display());
            println!("feature_ledger_md={}", ledger_md.display());
            println!(
                "feature_id={} summary succeeded={} failed={} skipped={} promotable={}",
                contract.feature_id, succeeded, failed, skipped, ledger.summary.promotable
            );
            if !ledger.summary.promotion_reasons.is_empty() {
                println!(
                    "feature_promotion_reasons={}",
                    ledger.summary.promotion_reasons.join(" | ")
                );
            }
            if aborted_on_failure {
                anyhow::bail!(
                    "Feature dispatch stopped early due to failed task (use --continue-on-failure to continue)."
                );
            }
            if failed > 0 {
                anyhow::bail!("Feature dispatch completed with {} failed task(s).", failed);
            }
            if !ledger.summary.acceptance_satisfied {
                anyhow::bail!(
                    "Feature dispatch completed but acceptance criteria are not fully satisfied."
                );
            }
        }
    }
    Ok(())
}

fn blocked_dependency_reason(
    task: &feature_contract::FeatureTask,
    statuses: &std::collections::HashMap<String, FeatureRunStatus>,
) -> Option<String> {
    let mut blockers = Vec::new();
    for dep in &task.depends_on {
        match statuses.get(dep) {
            Some(FeatureRunStatus::Succeeded) => {}
            Some(FeatureRunStatus::Failed(reason)) => {
                blockers.push(format!("dependency '{}' failed ({})", dep, reason));
            }
            Some(FeatureRunStatus::Skipped(reason)) => {
                blockers.push(format!("dependency '{}' skipped ({})", dep, reason));
            }
            None => blockers.push(format!("dependency '{}' has no result", dep)),
        }
    }
    if blockers.is_empty() {
        None
    } else {
        Some(blockers.join("; "))
    }
}

fn build_feature_task_context(
    contract: &feature_contract::FeatureContract,
    task: &feature_contract::FeatureTask,
) -> String {
    let mut context = task.context.clone().unwrap_or_default();
    if !context.is_empty() {
        context.push('\n');
    }
    context.push_str(&format!(
        "[feature_id:{}]\n[feature_task_id:{}]",
        contract.feature_id, task.id
    ));
    if !task.depends_on.is_empty() {
        context.push_str(&format!(
            "\n[feature_depends_on:{}]",
            task.depends_on.join(",")
        ));
    }
    if !task.mapped_acceptance.is_empty() {
        context.push_str(&format!(
            "\n[feature_acceptance:{}]",
            task.mapped_acceptance.join(",")
        ));
    }
    if let Some(category) = task.auto_store.as_deref() {
        context.push_str(&format!("\n[auto_store:{}]", category));
    }
    if let Some(tool) = task.cli_tool.as_deref() {
        context.push_str(&format!("\n[cli_tool:{}]", tool));
    }
    if let Some(model) = task.cli_model.as_deref() {
        context.push_str(&format!("\n[cli_model:{}]", model));
    }
    context
}

fn print_feature_plan(contract: &feature_contract::FeatureContract) -> anyhow::Result<()> {
    let batches = contract.execution_batches()?;
    let acceptance = contract.acceptance_coverage();
    let acceptance_verify = contract.acceptance_verification_coverage();
    println!(
        "feature_id={} tasks_total={} tasks_enabled={} acceptance={} checks={} batches={}",
        contract.feature_id,
        contract.tasks.len(),
        contract.tasks.iter().filter(|t| t.enabled).count(),
        contract.acceptance_criteria.len(),
        contract.verification_checks.len(),
        batches.len()
    );
    for ac in &contract.acceptance_criteria {
        let covered_by = acceptance
            .get(&ac.id)
            .cloned()
            .unwrap_or_default()
            .join(",");
        println!(
            "acceptance={} covered_by={}",
            ac.id,
            if covered_by.is_empty() {
                "-".to_string()
            } else {
                covered_by
            }
        );
    }
    for ac in &contract.acceptance_criteria {
        let checks = acceptance_verify
            .get(&ac.id)
            .cloned()
            .unwrap_or_default()
            .join(",");
        println!(
            "acceptance_verify={} checks={}",
            ac.id,
            if checks.is_empty() {
                "-".to_string()
            } else {
                checks
            }
        );
    }
    for check in &contract.verification_checks {
        println!(
            "verification_check={} required={} acceptance={} command={}",
            check.id,
            check.required,
            check.mapped_acceptance.join(","),
            check.command
        );
    }
    for (idx, batch) in batches.iter().enumerate() {
        println!("batch={} tasks={}", idx + 1, batch.join(","));
        for task_id in batch {
            let task = contract
                .task_by_id(task_id)
                .ok_or_else(|| anyhow::anyhow!("task '{}' missing from contract", task_id))?;
            println!(
                "  - task={} ghost={} depends_on={} acceptance={} goal={}",
                task.id,
                task.ghost.as_deref().unwrap_or("auto"),
                if task.depends_on.is_empty() {
                    "-".to_string()
                } else {
                    task.depends_on.join(",")
                },
                task.mapped_acceptance.join(","),
                task.goal.replace('\n', " ")
            );
        }
    }
    Ok(())
}

fn build_feature_run_ledger(
    contract: &feature_contract::FeatureContract,
    contract_path: &std::path::Path,
    statuses: &std::collections::HashMap<String, FeatureRunStatus>,
    dispatch_ids: &std::collections::HashMap<String, String>,
    succeeded: usize,
    failed: usize,
    skipped: usize,
) -> FeatureRunLedger {
    let coverage = contract.acceptance_coverage();
    let mut acceptance_rows = Vec::new();
    let mut all_covered = true;
    let mut all_satisfied = true;

    for ac in &contract.acceptance_criteria {
        let covered_by_tasks = coverage.get(&ac.id).cloned().unwrap_or_default();
        let mut succeeded_tasks = Vec::new();
        for task_id in &covered_by_tasks {
            if matches!(statuses.get(task_id), Some(FeatureRunStatus::Succeeded)) {
                succeeded_tasks.push(task_id.clone());
            }
        }
        let covered = !covered_by_tasks.is_empty();
        let satisfied = !succeeded_tasks.is_empty();
        all_covered &= covered;
        all_satisfied &= satisfied;
        acceptance_rows.push(FeatureAcceptanceLedgerRow {
            acceptance_id: ac.id.clone(),
            covered_by_tasks,
            succeeded_tasks,
            covered,
            satisfied,
        });
    }
    let mut promotion_reasons = Vec::new();
    if failed > 0 {
        promotion_reasons.push(format!("{} task(s) failed", failed));
    }
    if skipped > 0 {
        promotion_reasons.push(format!("{} task(s) skipped", skipped));
    }
    if !all_covered {
        promotion_reasons.push("some acceptance criteria have no task coverage".to_string());
    }
    if !all_satisfied {
        promotion_reasons
            .push("some acceptance criteria have no succeeded mapped task".to_string());
    }

    let tasks = contract
        .tasks
        .iter()
        .filter(|t| t.enabled)
        .map(|t| {
            let status = statuses.get(&t.id).cloned().unwrap_or_else(|| {
                FeatureRunStatus::Skipped("not_run_due_to_early_stop".to_string())
            });
            FeatureTaskLedgerRow {
                task_id: t.id.clone(),
                dispatch_task_id: dispatch_ids.get(&t.id).cloned(),
                status: status.label().to_string(),
                reason: status.reason().map(|s| s.to_string()),
                mapped_acceptance: t.mapped_acceptance.clone(),
            }
        })
        .collect::<Vec<_>>();

    FeatureRunLedger {
        timestamp_utc: chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        feature_id: contract.feature_id.clone(),
        contract_path: contract_path.display().to_string(),
        tasks,
        acceptance: acceptance_rows,
        summary: FeatureRunSummary {
            succeeded,
            failed,
            skipped,
            acceptance_covered: all_covered,
            acceptance_satisfied: all_satisfied,
            promotable: failed == 0 && skipped == 0 && all_satisfied,
            promotion_reasons,
        },
    }
}

fn write_feature_ledger_artifacts(
    ledger: &FeatureRunLedger,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let out_dir = std::path::PathBuf::from("eval/results");
    std::fs::create_dir_all(&out_dir)?;
    let safe_feature_id = ledger
        .feature_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let base = format!("feature-{}-{}", safe_feature_id, ledger.timestamp_utc);
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    std::fs::write(&json_path, serde_json::to_string_pretty(ledger)?)?;
    std::fs::write(&md_path, render_feature_ledger_markdown(ledger))?;
    Ok((json_path, md_path))
}

fn render_feature_ledger_markdown(ledger: &FeatureRunLedger) -> String {
    let mut out = String::new();
    out.push_str("# Feature Run Ledger\n\n");
    out.push_str(&format!("- feature_id: `{}`\n", ledger.feature_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", ledger.timestamp_utc));
    out.push_str(&format!("- contract: `{}`\n", ledger.contract_path));
    out.push_str(&format!(
        "- summary: succeeded={} failed={} skipped={} acceptance_covered={} acceptance_satisfied={} promotable={}\n",
        ledger.summary.succeeded,
        ledger.summary.failed,
        ledger.summary.skipped,
        ledger.summary.acceptance_covered,
        ledger.summary.acceptance_satisfied,
        ledger.summary.promotable
    ));
    if !ledger.summary.promotion_reasons.is_empty() {
        out.push_str(&format!(
            "- promotion_reasons: {}\n",
            ledger.summary.promotion_reasons.join(" | ")
        ));
    }
    out.push('\n');

    out.push_str("## Tasks\n\n");
    out.push_str("| task_id | dispatch_task_id | status | reason | mapped_acceptance |\n");
    out.push_str("|---|---|---|---|---|\n");
    for row in &ledger.tasks {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.task_id,
            row.dispatch_task_id
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            row.status,
            row.reason.clone().unwrap_or_else(|| "-".to_string()),
            if row.mapped_acceptance.is_empty() {
                "-".to_string()
            } else {
                row.mapped_acceptance.join(",")
            }
        ));
    }

    out.push_str("\n## Acceptance\n\n");
    out.push_str("| acceptance_id | covered_by_tasks | succeeded_tasks | covered | satisfied |\n");
    out.push_str("|---|---|---|---|---|\n");
    for row in &ledger.acceptance {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.acceptance_id,
            if row.covered_by_tasks.is_empty() {
                "-".to_string()
            } else {
                row.covered_by_tasks.join(",")
            },
            if row.succeeded_tasks.is_empty() {
                "-".to_string()
            } else {
                row.succeeded_tasks.join(",")
            },
            row.covered,
            row.satisfied
        ));
    }
    out
}

fn run_feature_verify(
    contract: &feature_contract::FeatureContract,
    contract_path: &std::path::Path,
) -> anyhow::Result<FeatureVerifyLedger> {
    if contract.verification_checks.is_empty() {
        anyhow::bail!(
            "feature '{}' has no verification_checks; add checks mapped to acceptance criteria",
            contract.feature_id
        );
    }

    let mut check_rows = Vec::new();
    let mut checks_passed = 0usize;
    let mut checks_failed = 0usize;
    let mut required_checks_failed = 0usize;

    for check in &contract.verification_checks {
        let output = std::process::Command::new("zsh")
            .arg("-lc")
            .arg(&check.command)
            .output();
        let (status, exit_code, stdout_tail, stderr_tail) = match output {
            Ok(out) => {
                let ok = out.status.success();
                if ok {
                    checks_passed += 1;
                } else {
                    checks_failed += 1;
                    if check.required {
                        required_checks_failed += 1;
                    }
                }
                (
                    if ok { "passed" } else { "failed" }.to_string(),
                    out.status.code(),
                    tail_text(&String::from_utf8_lossy(&out.stdout), 500),
                    tail_text(&String::from_utf8_lossy(&out.stderr), 500),
                )
            }
            Err(e) => {
                checks_failed += 1;
                if check.required {
                    required_checks_failed += 1;
                }
                (
                    "error".to_string(),
                    None,
                    String::new(),
                    format!("failed to launch check command: {}", e),
                )
            }
        };
        check_rows.push(FeatureVerifyCheckRow {
            check_id: check.id.clone(),
            command: check.command.clone(),
            required: check.required,
            status,
            exit_code,
            mapped_acceptance: check.mapped_acceptance.clone(),
            stdout_tail,
            stderr_tail,
        });
    }

    let mut acceptance_rows = Vec::new();
    let verification_coverage = contract.acceptance_verification_coverage();
    let mut acceptance_satisfied = true;
    for ac in &contract.acceptance_criteria {
        let checks = verification_coverage
            .get(&ac.id)
            .cloned()
            .unwrap_or_default();
        let passed_checks = check_rows
            .iter()
            .filter(|c| c.status == "passed" && c.mapped_acceptance.iter().any(|id| id == &ac.id))
            .map(|c| c.check_id.clone())
            .collect::<Vec<_>>();
        let satisfied = !passed_checks.is_empty();
        acceptance_satisfied &= satisfied;
        acceptance_rows.push(FeatureVerifyAcceptanceRow {
            acceptance_id: ac.id.clone(),
            checks,
            passed_checks,
            satisfied,
        });
    }

    let mut promotion_reasons = Vec::new();
    if required_checks_failed > 0 {
        promotion_reasons.push(format!(
            "{} required verification check(s) failed",
            required_checks_failed
        ));
    }
    if !acceptance_satisfied {
        promotion_reasons
            .push("acceptance criteria are not satisfied by passing checks".to_string());
    }
    let promotable = required_checks_failed == 0 && acceptance_satisfied;

    Ok(FeatureVerifyLedger {
        timestamp_utc: chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        feature_id: contract.feature_id.clone(),
        contract_path: contract_path.display().to_string(),
        checks: check_rows,
        acceptance: acceptance_rows,
        summary: FeatureVerifySummary {
            checks_total: checks_passed + checks_failed,
            checks_passed,
            checks_failed,
            required_checks_failed,
            acceptance_satisfied,
            promotable,
            promotion_reasons,
        },
    })
}

fn write_feature_verify_artifacts(
    ledger: &FeatureVerifyLedger,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let out_dir = std::path::PathBuf::from("eval/results");
    std::fs::create_dir_all(&out_dir)?;
    let safe_feature_id = ledger
        .feature_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let base = format!(
        "feature-verify-{}-{}",
        safe_feature_id, ledger.timestamp_utc
    );
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    std::fs::write(&json_path, serde_json::to_string_pretty(ledger)?)?;
    std::fs::write(&md_path, render_feature_verify_markdown(ledger))?;
    Ok((json_path, md_path))
}

fn render_feature_verify_markdown(ledger: &FeatureVerifyLedger) -> String {
    let mut out = String::new();
    out.push_str("# Feature Verify Ledger\n\n");
    out.push_str(&format!("- feature_id: `{}`\n", ledger.feature_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", ledger.timestamp_utc));
    out.push_str(&format!("- contract: `{}`\n", ledger.contract_path));
    out.push_str(&format!(
        "- summary: checks_total={} checks_passed={} checks_failed={} required_checks_failed={} acceptance_satisfied={} promotable={}\n",
        ledger.summary.checks_total,
        ledger.summary.checks_passed,
        ledger.summary.checks_failed,
        ledger.summary.required_checks_failed,
        ledger.summary.acceptance_satisfied,
        ledger.summary.promotable
    ));
    if !ledger.summary.promotion_reasons.is_empty() {
        out.push_str(&format!(
            "- promotion_reasons: {}\n",
            ledger.summary.promotion_reasons.join(" | ")
        ));
    }
    out.push('\n');

    out.push_str("## Checks\n\n");
    out.push_str("| check_id | status | required | exit_code | mapped_acceptance | command |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for row in &ledger.checks {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | `{}` |\n",
            row.check_id,
            row.status,
            row.required,
            row.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if row.mapped_acceptance.is_empty() {
                "-".to_string()
            } else {
                row.mapped_acceptance.join(",")
            },
            row.command.replace('|', "\\|")
        ));
    }

    out.push_str("\n## Acceptance\n\n");
    out.push_str("| acceptance_id | checks | passed_checks | satisfied |\n");
    out.push_str("|---|---|---|---|\n");
    for row in &ledger.acceptance {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            row.acceptance_id,
            if row.checks.is_empty() {
                "-".to_string()
            } else {
                row.checks.join(",")
            },
            if row.passed_checks.is_empty() {
                "-".to_string()
            } else {
                row.passed_checks.join(",")
            },
            row.satisfied
        ));
    }
    out
}

fn tail_text(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return input.trim().to_string();
    }
    let start = chars.len() - max_chars;
    chars.drain(..start);
    chars.into_iter().collect::<String>().trim().to_string()
}

fn handle_memory(
    action: MemoryAction,
    memory: &MemoryStore,
    embedder: Option<&Embedder>,
) -> anyhow::Result<()> {
    match action {
        MemoryAction::List => {
            let memories = memory.list()?;
            if memories.is_empty() {
                println!("No active memories.");
            } else {
                for m in &memories {
                    println!("[{}] {} — {}", m.id[..8].to_string(), m.category, m.content);
                }
                println!("\n{} memories total.", memories.len());
            }
        }
        MemoryAction::Add { category, content } => {
            let embedding = embedder.and_then(|e| e.embed(&content).ok());
            let id = memory.store(&category, &content, embedding.as_deref())?;
            println!("Stored memory: {}", &id[..8]);
        }
        MemoryAction::Retire { id } => {
            let memories = memory.list()?;
            let full_id = memories
                .iter()
                .find(|m| m.id.starts_with(&id))
                .map(|m| m.id.clone());

            if let Some(full_id) = full_id {
                memory.retire(&full_id)?;
                println!("Retired memory: {}", &full_id[..8]);
            } else {
                println!("Memory not found: {}", id);
            }
        }
    }
    Ok(())
}

fn handle_jobs(action: JobsAction, handle: &core::CoreHandle) -> anyhow::Result<()> {
    let engine = handle
        .cron_engine
        .as_ref()
        .expect("Cron engine not initialized");
    match action {
        JobsAction::List => {
            let jobs = engine.list_jobs()?;
            if jobs.is_empty() {
                println!("No scheduled jobs.");
            } else {
                for j in &jobs {
                    let status = if j.enabled { "on" } else { "off" };
                    let next = j
                        .next_run
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "  [{}] {} ({}) — next: {} — {}",
                        &j.id[..8],
                        j.name,
                        status,
                        next,
                        j.prompt
                    );
                }
            }
        }
        JobsAction::Add {
            name,
            every,
            cron,
            prompt,
        } => {
            let schedule = if let Some(secs) = every {
                Schedule::Interval {
                    every_secs: secs,
                    jitter: 0.1,
                }
            } else if let Some(expr) = cron {
                Schedule::Cron { expression: expr }
            } else {
                eprintln!("Specify --every <secs> or --cron <expression>");
                return Ok(());
            };
            let id = engine.create_job(&name, schedule, &prompt, None)?;
            println!("Created job: {} ({})", name, &id[..8]);
        }
        JobsAction::Delete { id } => {
            let jobs = engine.list_jobs()?;
            let full_id = jobs
                .iter()
                .find(|j| j.id.starts_with(&id))
                .map(|j| j.id.clone());
            if let Some(full_id) = full_id {
                engine.delete_job(&full_id)?;
                println!("Deleted job: {}", &full_id[..8]);
            } else {
                println!("Job not found: {}", id);
            }
        }
    }
    Ok(())
}

async fn run_dispatch(
    config: Config,
    memory: Arc<MemoryStore>,
    goal: String,
    context: Option<String>,
    ghost: Option<String>,
    auto_store: Option<String>,
    cli_tool: Option<String>,
    cli_model: Option<String>,
    wait_secs: u64,
    lane: String,
    risk: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    validate_lane(&lane)?;
    validate_risk(&risk)?;
    let repo = repo.unwrap_or_else(kpi::default_repo_name);
    let config_for_finalize = config.clone();
    let handle = AthenaCore::start(config, memory).await?;
    if cli_tool.is_some() || cli_model.is_some() {
        let mut knobs = handle
            .knobs
            .write()
            .map_err(|_| anyhow::anyhow!("Failed to lock runtime knobs"))?;
        if let Some(tool) = cli_tool.as_deref() {
            knobs.set("cli_tool", tool).map_err(anyhow::Error::msg)?;
            eprintln!("Dispatch override: cli_tool={}", tool);
        }
        if let Some(model) = cli_model.as_deref() {
            knobs.set("cli_model", model).map_err(anyhow::Error::msg)?;
            eprintln!("Dispatch override: cli_model={}", model);
        }
    }
    let context = dispatch_context(context, auto_store);

    // CLI dispatch waits on the delivered broadcast receiver, so target
    // broadcast to guarantee result pulses are observable by this command.
    let target = crate::pulse::PulseTarget::Broadcast;

    let mut pulse_rx = handle.pulse_bus.subscribe();
    let ghost_label = ghost.clone().unwrap_or_else(|| "auto".to_string());
    let task_id = handle
        .dispatch_task(core::AutonomousTask {
            goal: goal.clone(),
            context,
            ghost,
            target,
            lane,
            risk_tier: risk,
            repo,
            task_id: None,
        })
        .await?;

    eprintln!(
        "Dispatched autonomous task to {} (task_id={}). Waiting up to {}s...",
        ghost_label, task_id, wait_secs
    );
    match wait_for_autonomous_pulse(&mut pulse_rx, &task_id, wait_secs).await {
        WaitForAutonomousOutcome::Received => Ok(()),
        WaitForAutonomousOutcome::TimedOut => {
            mark_dispatch_task_failed_if_started(
                &config_for_finalize,
                &task_id,
                OUTCOME_REASON_DISPATCH_TIMEOUT,
            );
            Ok(())
        }
        WaitForAutonomousOutcome::ChannelClosed => {
            mark_dispatch_task_failed_if_started(
                &config_for_finalize,
                &task_id,
                OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED,
            );
            Ok(())
        }
    }
}

fn dispatch_context(context: Option<String>, auto_store: Option<String>) -> String {
    let mut context = context.unwrap_or_default();
    if let Some(category) = auto_store {
        if !context.is_empty() {
            context.push('\n');
        }
        context.push_str(&format!("[auto_store:{}]", category));
    }
    context
}

async fn wait_for_autonomous_pulse(
    rx: &mut tokio::sync::broadcast::Receiver<crate::pulse::Pulse>,
    task_id: &str,
    wait_secs: u64,
) -> WaitForAutonomousOutcome {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(wait_secs);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            eprintln!(
                "Timed out waiting for autonomous task result pulse (task_id={}).",
                task_id
            );
            return WaitForAutonomousOutcome::TimedOut;
        }
        let remaining = deadline.duration_since(now);
        let pulse = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(pulse)) => pulse,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                eprintln!(
                    "Pulse stream lagged by {} events while waiting for task_id={}; continuing...",
                    n, task_id
                );
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                eprintln!("Pulse channel closed before a result was delivered.");
                return WaitForAutonomousOutcome::ChannelClosed;
            }
            Err(_) => {
                eprintln!(
                    "Timed out waiting for autonomous task result pulse (task_id={}).",
                    task_id
                );
                return WaitForAutonomousOutcome::TimedOut;
            }
        };
        if pulse_matches_task_id(&pulse, task_id) {
            println!("[{}] {}", pulse.source.label(), pulse.content);
            return WaitForAutonomousOutcome::Received;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitForAutonomousOutcome {
    Received,
    TimedOut,
    ChannelClosed,
}

fn mark_dispatch_task_failed_if_started(config: &Config, task_id: &str, reason: &str) {
    let conn = match kpi::open_connection(config) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!(
                "Failed to open DB while finalizing timed-out task_id={}: {}",
                task_id, e
            );
            return;
        }
    };
    let store = kpi::TaskOutcomeStore::new(conn);
    match store.fail_task_if_started(task_id, reason) {
        Ok(true) => eprintln!(
            "Marked task_id={} as failed because no terminal pulse was observed: {}",
            task_id, reason
        ),
        Ok(false) => {}
        Err(e) => eprintln!(
            "Failed to finalize timed-out task_id={} in outcomes table: {}",
            task_id, e
        ),
    }
}

async fn wait_for_terminal_outcome_status(
    config: &Config,
    task_id: &str,
    wait_secs: u64,
) -> anyhow::Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(wait_secs);
    loop {
        match read_task_outcome_status(config, task_id)? {
            Some(status) if status != "started" => return Ok(Some(status)),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

fn read_task_outcome_status(config: &Config, task_id: &str) -> anyhow::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let conn = kpi::open_connection(config)?;
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM autonomous_task_outcomes WHERE task_id = ?1",
            rusqlite::params![task_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(status)
}

fn pulse_matches_task_id(pulse: &crate::pulse::Pulse, task_id: &str) -> bool {
    matches!(pulse.source, crate::pulse::PulseSource::AutonomousTask)
        && pulse.task_id.as_deref() == Some(task_id)
}

async fn run_observe() -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt;

    let path = observer::socket_path();

    loop {
        eprintln!("\x1b[2mConnecting to {}...\x1b[0m", path.display());

        let stream = match tokio::net::UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(_) => {
                eprintln!("\x1b[2mWaiting for Athena...\x1b[0m");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        eprintln!("\x1b[1;32mConnected.\x1b[0m Streaming events...\n");

        let reader = tokio::io::BufReader::new(stream);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<observer::ObserverEvent>(&line) {
                Ok(event) => println!("{}", event.format_colored()),
                Err(_) => println!("{}", line),
            }
        }

        eprintln!("\n\x1b[2mConnection lost. Reconnecting...\x1b[0m");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

enum ChatCommandOutcome {
    Continue,
    Exit,
    SendToCore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatCommand {
    Set,
    Exit,
    Help,
    Ghosts,
    Memories,
    Mood,
    Jobs,
    Models,
    Model,
    ModelSet,
    CliModel,
    CliModelSet,
    Chat,
}

fn classify_chat_command(input: &str) -> ChatCommand {
    if input.starts_with("/set") {
        return ChatCommand::Set;
    }
    match input {
        "/quit" | "/exit" | "/q" => ChatCommand::Exit,
        "/help" | "/h" => ChatCommand::Help,
        "/ghosts" => ChatCommand::Ghosts,
        "/memories" => ChatCommand::Memories,
        "/mood" => ChatCommand::Mood,
        "/jobs" => ChatCommand::Jobs,
        "/models" => ChatCommand::Models,
        "/model" => ChatCommand::Model,
        "/cli_model" => ChatCommand::CliModel,
        _ if input.starts_with("/model ") => ChatCommand::ModelSet,
        _ if input.starts_with("/cli_model ") => ChatCommand::CliModelSet,
        _ => ChatCommand::Chat,
    }
}

fn print_cli_help() {
    println!("Commands:");
    println!("  /ghosts    — List active ghosts");
    println!("  /memories  — List saved memories");
    println!("  /model     — Show/switch LLM model");
    println!("  /model <name>  — Switch LLM model");
    println!("  /models    — List available models from API");
    println!("  /cli_model — Show/switch model for CLI tools (Claude Code, Codex, OpenCode)");
    println!("  /cli_model <name> — Set CLI tool model");
    println!("  /cli_model reset  — Reset to tool default");
    println!("  /set       — Show/change runtime knobs");
    println!("  /mood      — Show current mood");
    println!("  /jobs      — List scheduled jobs");
    println!("  /help      — This help");
    println!("  /quit      — Exit");
}

fn handle_set_command(input: &str, handle: &core::CoreHandle) {
    let parts: Vec<&str> = input.split_whitespace().collect();
    match parts.len() {
        1 => {
            let k = handle.knobs.read().unwrap();
            println!("{}", k.display());
        }
        3 => {
            let mut k = handle.knobs.write().unwrap();
            match k.set(parts[1], parts[2]) {
                Ok(msg) => {
                    println!("{}", msg);
                    handle.observer.emit(observer::ObserverEvent::new(
                        ObserverCategory::KnobChange,
                        format!("{} = {}", parts[1], parts[2]),
                    ));
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
        _ => eprintln!("Usage: /set OR /set <key> <value>"),
    }
}

fn print_ghosts(handle: &core::CoreHandle) {
    for g in handle.list_ghosts() {
        println!("  {} — {} [{}]", g.name, g.description, g.tools.join(", "));
    }
}

fn print_memories(handle: &core::CoreHandle) {
    match handle.list_memories() {
        Ok(memories) if memories.is_empty() => println!("No memories."),
        Ok(memories) => {
            for m in &memories {
                println!("  [{}] {} — {}", &m.id[..8], m.category, m.content);
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

fn print_jobs(handle: &core::CoreHandle) {
    if let Some(engine) = &handle.cron_engine {
        match engine.list_jobs() {
            Ok(jobs) if jobs.is_empty() => println!("No scheduled jobs."),
            Ok(jobs) => {
                for j in &jobs {
                    let status = if j.enabled { "on" } else { "off" };
                    let next = j
                        .next_run
                        .map(|t| t.format("%H:%M").to_string())
                        .unwrap_or_else(|| "-".to_string());
                    println!("  [{}] {} ({}) next: {}", &j.id[..8], j.name, status, next);
                }
            }
            Err(e) => eprintln!("Error: {}", e),
        }
    }
}

async fn print_models(handle: &core::CoreHandle) {
    match handle.llm.list_models().await {
        Ok(models) if models.is_empty() => println!("No models returned by API."),
        Ok(models) => {
            let current = handle.llm.current_model();
            println!("Available models:");
            for m in &models {
                if *m == current {
                    println!("  {} (active)", m);
                } else {
                    println!("  {}", m);
                }
            }
        }
        Err(e) => eprintln!("Error listing models: {}", e),
    }
}

fn handle_model_command(input: &str, handle: &core::CoreHandle) -> bool {
    if input == "/model" {
        println!("Current model: {}", handle.llm.current_model());
        return true;
    }
    if let Some(arg) = input.strip_prefix("/model ") {
        let arg = arg.trim();
        if arg == "reset" {
            handle.llm.set_model_override(None);
            println!("Reset to default model: {}", handle.llm.current_model());
        } else {
            handle.llm.set_model_override(Some(arg.to_string()));
            println!("Model set to: {}", arg);
        }
        return true;
    }
    false
}

fn handle_cli_model_command(input: &str, handle: &core::CoreHandle) -> bool {
    if input == "/cli_model" {
        let model = handle.knobs.read().unwrap().cli_model.clone();
        if model.is_empty() {
            println!("CLI tool model: default (tool decides)");
        } else {
            println!("CLI tool model: {}", model);
        }
        return true;
    }
    if let Some(arg) = input.strip_prefix("/cli_model ") {
        let arg = arg.trim();
        let mut k = handle.knobs.write().unwrap();
        match k.set("cli_model", arg) {
            Ok(msg) => println!("{}", msg),
            Err(e) => eprintln!("Error: {}", e),
        }
        return true;
    }
    false
}

async fn handle_chat_command(input: &str, handle: &core::CoreHandle) -> ChatCommandOutcome {
    match classify_chat_command(input) {
        ChatCommand::Set => {
            handle_set_command(input, handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::Exit => ChatCommandOutcome::Exit,
        ChatCommand::Help => {
            print_cli_help();
            ChatCommandOutcome::Continue
        }
        ChatCommand::Ghosts => {
            print_ghosts(handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::Memories => {
            print_memories(handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::Mood => {
            println!("{}", handle.mood.describe());
            ChatCommandOutcome::Continue
        }
        ChatCommand::Jobs => {
            print_jobs(handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::Models => {
            print_models(handle).await;
            ChatCommandOutcome::Continue
        }
        ChatCommand::Model | ChatCommand::ModelSet => {
            let _ = handle_model_command(input, handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::CliModel | ChatCommand::CliModelSet => {
            let _ = handle_cli_model_command(input, handle);
            ChatCommandOutcome::Continue
        }
        ChatCommand::Chat => ChatCommandOutcome::SendToCore,
    }
}

fn spawn_delivered_pulse_logger(handle: &core::CoreHandle) {
    let delivered_rx = handle.delivered_rx.clone();
    tokio::spawn(async move {
        let mut rx = delivered_rx.lock().await;
        while let Some(pulse) = rx.recv().await {
            eprintln!(
                "\n\x1b[2;36m[{}] {}\x1b[0m",
                pulse.source.label(),
                pulse.content
            );
            eprint!("you> ");
        }
    });
}

fn chat_history_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".athena").join("history.txt"))
        .unwrap_or_else(|| PathBuf::from(".athena_history"))
}

fn save_cli_history(rl: &mut rustyline::DefaultEditor, history_path: &std::path::Path) {
    let _ = rl.save_history(history_path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if history_path.exists() {
            let _ = std::fs::set_permissions(history_path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn print_tool_run(tool: &str, result: &str, success: bool) {
    let icon = if success { "\u{2705}" } else { "\u{274c}" };
    let body = result
        .strip_prefix("[tool result]\n")
        .or_else(|| result.strip_prefix("[tool error]\n"))
        .unwrap_or(result);
    let preview = if body.len() > 200 {
        format!(
            "{}... [{} chars]",
            &body[..body.floor_char_boundary(200)],
            body.len()
        )
    } else {
        body.to_string()
    };
    eprintln!("  {} {} → {}", icon, tool, preview.replace('\n', " "));
}

async fn stream_cli_events(mut events: tokio::sync::mpsc::Receiver<CoreEvent>) {
    let mut streaming = false;
    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Status(s) => eprintln!("  {}", s),
            CoreEvent::StreamChunk(chunk) => {
                use std::io::Write;
                if !streaming {
                    streaming = true;
                    print!("\n");
                }
                print!("{}", chunk);
                let _ = std::io::stdout().flush();
            }
            CoreEvent::ToolRun {
                tool,
                result,
                success,
            } => print_tool_run(&tool, &result, success),
            CoreEvent::Response(r) => {
                if streaming {
                    println!("\n");
                } else {
                    println!("\n{}\n", r);
                }
            }
            CoreEvent::Error(e) => {
                if streaming {
                    println!();
                }
                if e.contains("cancelled") {
                    println!("Action cancelled.");
                } else {
                    eprintln!("Error: {}", e);
                }
            }
            CoreEvent::Pulse(p) => println!("\n[pulse] {}\n", p),
        }
    }
}

async fn run_chat(
    config: Config,
    memory: Arc<MemoryStore>,
    auto_approve: bool,
) -> anyhow::Result<()> {
    let handle = AthenaCore::start(config, memory).await?;
    let confirmer: Arc<dyn confirm::Confirmer> = Arc::new(CliConfirmer { auto_approve });

    let session = SessionContext {
        platform: "cli".into(),
        user_id: "local".into(),
        chat_id: "local".into(),
    };

    eprintln!("Athena ready. Type /help for commands.\n");

    let history_path = chat_history_path();

    let mut rl = rustyline::DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    spawn_delivered_pulse_logger(&handle);

    loop {
        let line = match rl.readline("you> ") {
            Ok(line) => line,
            Err(
                rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof,
            ) => {
                break;
            }
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        rl.add_history_entry(input)?;

        match handle_chat_command(input, &handle).await {
            ChatCommandOutcome::Continue => continue,
            ChatCommandOutcome::Exit => break,
            ChatCommandOutcome::SendToCore => {}
        }

        let events = match handle.chat(session.clone(), input, confirmer.clone()).await {
            Ok(rx) => rx,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };
        stream_cli_events(events).await;
    }

    save_cli_history(&mut rl, &history_path);

    eprintln!("Goodbye.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_feature_run_ledger, classify_chat_command, pulse_matches_task_id, run_feature_verify,
        wait_for_autonomous_pulse, ChatCommand, FeatureRunStatus, WaitForAutonomousOutcome,
    };
    use crate::feature_contract::{
        AcceptanceCriterion, FeatureContract, FeatureTask, VerificationCheck,
    };
    use crate::pulse::{Pulse, PulseSource, Urgency};
    use std::collections::HashMap;
    use std::path::Path;

    #[test]
    fn classify_exit_aliases() {
        assert_eq!(classify_chat_command("/quit"), ChatCommand::Exit);
        assert_eq!(classify_chat_command("/exit"), ChatCommand::Exit);
        assert_eq!(classify_chat_command("/q"), ChatCommand::Exit);
    }

    #[test]
    fn classify_model_commands() {
        assert_eq!(classify_chat_command("/model"), ChatCommand::Model);
        assert_eq!(classify_chat_command("/model reset"), ChatCommand::ModelSet);
        assert_eq!(classify_chat_command("/cli_model"), ChatCommand::CliModel);
        assert_eq!(
            classify_chat_command("/cli_model gpt-5-codex"),
            ChatCommand::CliModelSet
        );
    }

    #[test]
    fn classify_set_and_default_chat() {
        assert_eq!(classify_chat_command("/set"), ChatCommand::Set);
        assert_eq!(
            classify_chat_command("/set temperature 0.2"),
            ChatCommand::Set
        );
        assert_eq!(
            classify_chat_command("please summarize this"),
            ChatCommand::Chat
        );
    }

    #[test]
    fn pulse_match_requires_task_id_and_source() {
        let p = Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "ok".into())
            .with_task_id("task-123");
        assert!(pulse_matches_task_id(&p, "task-123"));
        assert!(!pulse_matches_task_id(&p, "task-999"));

        let non_auto = Pulse::new(PulseSource::Heartbeat, Urgency::Medium, "noop".into())
            .with_task_id("task-123");
        assert!(!pulse_matches_task_id(&non_auto, "task-123"));
    }

    #[tokio::test]
    async fn wait_for_autonomous_pulse_correlates_by_task_id() {
        let (tx, _) = tokio::sync::broadcast::channel(8);
        let mut rx = tx.subscribe();

        let _ = tx.send(
            Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "other".into())
                .with_task_id("task-other"),
        );
        let _ = tx.send(
            Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "match".into())
                .with_task_id("task-match"),
        );

        let res = wait_for_autonomous_pulse(&mut rx, "task-match", 1).await;
        assert_eq!(res, WaitForAutonomousOutcome::Received);
    }

    #[tokio::test]
    async fn wait_for_autonomous_pulse_times_out_without_matching_pulse() {
        let (tx, _) = tokio::sync::broadcast::channel(8);
        let mut rx = tx.subscribe();
        let _ = tx.send(
            Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "other".into())
                .with_task_id("task-other"),
        );
        let res = wait_for_autonomous_pulse(&mut rx, "task-match", 0).await;
        assert_eq!(res, WaitForAutonomousOutcome::TimedOut);
    }

    #[tokio::test]
    async fn wait_for_autonomous_pulse_reports_channel_closed() {
        let (tx, _) = tokio::sync::broadcast::channel(8);
        let mut rx = tx.subscribe();
        drop(tx);
        let res = wait_for_autonomous_pulse(&mut rx, "task-match", 1).await;
        assert_eq!(res, WaitForAutonomousOutcome::ChannelClosed);
    }

    fn sample_contract() -> FeatureContract {
        FeatureContract {
            feature_id: "feat".to_string(),
            lane: Some("delivery".to_string()),
            risk: Some("low".to_string()),
            repo: Some("athena".to_string()),
            acceptance_criteria: vec![
                AcceptanceCriterion {
                    id: "AC-1".to_string(),
                    description: None,
                },
                AcceptanceCriterion {
                    id: "AC-2".to_string(),
                    description: None,
                },
            ],
            verification_checks: vec![
                VerificationCheck {
                    id: "V1".to_string(),
                    command: "true".to_string(),
                    mapped_acceptance: vec!["AC-1".to_string()],
                    required: true,
                },
                VerificationCheck {
                    id: "V2".to_string(),
                    command: "true".to_string(),
                    mapped_acceptance: vec!["AC-2".to_string()],
                    required: true,
                },
            ],
            tasks: vec![
                FeatureTask {
                    id: "T1".to_string(),
                    goal: "task1".to_string(),
                    context: None,
                    ghost: None,
                    lane: None,
                    risk: None,
                    repo: None,
                    auto_store: None,
                    wait_secs: None,
                    cli_tool: None,
                    cli_model: None,
                    mapped_acceptance: vec!["AC-1".to_string()],
                    depends_on: vec![],
                    enabled: true,
                },
                FeatureTask {
                    id: "T2".to_string(),
                    goal: "task2".to_string(),
                    context: None,
                    ghost: None,
                    lane: None,
                    risk: None,
                    repo: None,
                    auto_store: None,
                    wait_secs: None,
                    cli_tool: None,
                    cli_model: None,
                    mapped_acceptance: vec!["AC-2".to_string()],
                    depends_on: vec!["T1".to_string()],
                    enabled: true,
                },
            ],
        }
    }

    #[test]
    fn feature_ledger_marks_unsatisfied_acceptance() {
        let contract = sample_contract();
        let mut statuses = HashMap::new();
        statuses.insert("T1".to_string(), FeatureRunStatus::Succeeded);
        statuses.insert(
            "T2".to_string(),
            FeatureRunStatus::Failed("terminal_status=failed".to_string()),
        );
        let ledger = build_feature_run_ledger(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            &statuses,
            &HashMap::new(),
            1,
            1,
            0,
        );
        assert!(!ledger.summary.acceptance_satisfied);
        assert!(!ledger.summary.promotable);
    }

    #[test]
    fn feature_ledger_marks_promotable_when_all_acceptance_satisfied() {
        let contract = sample_contract();
        let mut statuses = HashMap::new();
        statuses.insert("T1".to_string(), FeatureRunStatus::Succeeded);
        statuses.insert("T2".to_string(), FeatureRunStatus::Succeeded);
        let ledger = build_feature_run_ledger(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            &statuses,
            &HashMap::new(),
            2,
            0,
            0,
        );
        assert!(ledger.summary.acceptance_satisfied);
        assert!(ledger.summary.promotable);
    }

    #[test]
    fn feature_verify_fails_when_required_check_fails() {
        let mut contract = sample_contract();
        contract.verification_checks[1].command = "false".to_string();
        let ledger =
            run_feature_verify(&contract, Path::new("eval/feature-contract-example.yaml")).unwrap();
        assert_eq!(ledger.summary.required_checks_failed, 1);
        assert!(!ledger.summary.promotable);
    }

    #[test]
    fn feature_verify_fails_when_acceptance_has_no_passing_check() {
        let mut contract = sample_contract();
        contract.verification_checks[1].command = "false".to_string();
        let ledger =
            run_feature_verify(&contract, Path::new("eval/feature-contract-example.yaml")).unwrap();
        assert!(!ledger.summary.acceptance_satisfied);
        assert!(ledger
            .summary
            .promotion_reasons
            .iter()
            .any(|r| r.contains("acceptance criteria are not satisfied")));
    }
}
