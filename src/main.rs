
mod config;
mod confirm;
mod ci_monitor;
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
mod ouath;
mod proactive;
mod profiles;
mod pulse;
mod randomness;
mod scheduler;
mod self_heal;
mod strategy;
mod secrets;
#[cfg(feature = "telegram")]
mod telegram;
mod ticket_intake;
mod tool_usage;
mod tools;

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
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
    /// Manage secrets stored in OS keyring
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },
    /// Run as a Telegram bot (requires --features telegram)
    #[cfg(feature = "telegram")]
    Telegram,
    /// Authenticate with OpenAI subscription (Ouath)
    Ouath {
        #[command(subcommand)]
        action: OuathAction,
    },
    /// Watch internal observer events in real time
    Observe,
    /// Monitor CI status for a PR and optionally heal/merge
    CiMonitor {
        /// PR URL to monitor
        #[arg(long)]
        pr: String,
        /// Optional PR branch name (auto-detected when omitted)
        #[arg(long)]
        branch: Option<String>,
        /// Auto-merge after CI passes
        #[arg(long)]
        auto_merge: bool,
        /// Attempt self-heal on CI failures
        #[arg(long)]
        heal: bool,
        /// Max self-heal attempts
        #[arg(long, default_value_t = ci_monitor::CI_HEAL_MAX_ATTEMPTS)]
        max_heal: u8,
        /// Seconds between CI polls
        #[arg(long, default_value_t = ci_monitor::CI_POLL_INTERVAL_SECS)]
        poll_interval: u64,
        /// Max total wait time in seconds
        #[arg(long, default_value_t = ci_monitor::CI_POLL_TIMEOUT_SECS)]
        timeout: u64,
    },
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
    /// Run supervised self-build pipeline in an isolated worktree
    SelfBuild {
        #[command(subcommand)]
        action: SelfBuildAction,
    },
}

#[derive(Subcommand)]
enum OuathAction {
    /// Start OAuth login flow
    Login,
    /// Show current authentication status
    Status,
    /// Remove cached tokens
    Logout,
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
enum SecretsAction {
    /// List known secret slots and their status
    List,
    /// Store a secret in the OS keyring
    Set {
        /// Secret key (e.g. github.token)
        key: String,
    },
    /// Delete a secret from the OS keyring
    Delete {
        /// Secret key (e.g. github.token)
        key: String,
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
        /// Fire once at an absolute time (RFC3339) or relative duration (e.g., 2h30m)
        #[arg(long)]
        at: Option<String>,
        /// Prompt to send to LLM when the job fires
        #[arg(long)]
        prompt: String,
        /// Route through a specific ghost
        #[arg(long)]
        ghost: Option<String>,
        /// Delivery target (broadcast | session:<platform>:<user_id>:<chat_id>)
        #[arg(long, default_value = "broadcast")]
        target: String,
    },
    /// Delete a job by ID
    Delete {
        /// Job ID (prefix match)
        id: String,
    },
    /// Enable a job by ID
    Enable {
        /// Job ID (prefix match)
        id: String,
    },
    /// Disable a job by ID
    Disable {
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
        /// Verification profile to run
        #[arg(long, default_value = "strict", value_parser = ["fast", "strict"])]
        profile: String,
    },
    /// Produce supervised promotion decision from latest dispatch/verify ledgers
    Promote {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
        /// Optional dispatch ledger JSON path (auto-detected when omitted)
        #[arg(long)]
        dispatch_ledger: Option<PathBuf>,
        /// Optional verify ledger JSON path (auto-detected when omitted)
        #[arg(long)]
        verify_ledger: Option<PathBuf>,
    },
    /// Dispatch feature tasks using DAG order and wait for terminal outcomes
    Dispatch {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
        /// Global wait timeout per task (seconds) when task-level wait_secs is unset
        #[arg(long, default_value_t = 180)]
        wait_secs: u64,
        /// Optional grace window (seconds) to poll DB terminal outcome after pulse wait timeout.
        /// If omitted, Athena computes adaptive grace from risk tier and task profile.
        #[arg(long)]
        outcome_grace_secs: Option<u64>,
        /// Continue dispatching independent tasks after failures
        #[arg(long)]
        continue_on_failure: bool,
        /// Revert commits from succeeded tasks when any task fails
        #[arg(long)]
        rollback_on_failure: bool,
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
    /// Run dispatch + verify + promote in one command and emit consolidated gate artifact
    Gate {
        /// Path to feature contract file
        #[arg(long)]
        file: PathBuf,
        /// Global wait timeout per task (seconds) when task-level wait_secs is unset
        #[arg(long, default_value_t = 180)]
        wait_secs: u64,
        /// Optional grace window override in seconds (adaptive when omitted)
        #[arg(long)]
        outcome_grace_secs: Option<u64>,
        /// Continue dispatching independent tasks after failures (recommended for full gate signal)
        #[arg(long, default_value_t = true)]
        continue_on_failure: bool,
        /// Optional CLI tool override for this run
        #[arg(long, value_parser = ["claude_code", "codex", "opencode"])]
        cli_tool: Option<String>,
        /// Optional coding model override for this run
        #[arg(long)]
        cli_model: Option<String>,
        /// Override lane for all tasks
        #[arg(long)]
        lane: Option<String>,
        /// Override risk tier for all tasks and promotion policy
        #[arg(long)]
        risk: Option<String>,
        /// Override repo/product label for all tasks
        #[arg(long)]
        repo: Option<String>,
        /// Verification profile to run
        #[arg(long, default_value = "strict", value_parser = ["fast", "strict"])]
        verify_profile: String,
    },
}

#[derive(Subcommand)]
enum SelfBuildAction {
    /// Execute one supervised self-build run from a ticket description
    Run {
        /// Ticket/problem statement Athena should fix
        #[arg(long)]
        ticket: String,
        /// Optional extra context (scope hints, constraints, links)
        #[arg(long)]
        context: Option<String>,
        /// Risk tier for promotion policy
        #[arg(long, default_value = "low")]
        risk: String,
        /// Autonomous dispatch wait timeout (seconds)
        #[arg(long, default_value_t = 300)]
        wait_secs: u64,
        /// Optional CLI tool override for coding execution
        #[arg(long, value_parser = ["claude_code", "codex", "opencode"])]
        cli_tool: Option<String>,
        /// Optional CLI model override for coding execution
        #[arg(long)]
        cli_model: Option<String>,
        /// Maintenance pack profile
        #[arg(long, default_value = "rust", value_parser = ["rust", "generic"])]
        maintenance_profile: String,
        /// Keep the isolated worktree after run
        #[arg(long)]
        keep_worktree: bool,
        /// Allow auto-promotion recommendation for eligible low-risk runs
        #[arg(long)]
        allow_auto_promote: bool,
        /// Promotion execution mode after a green run
        #[arg(long, value_enum, default_value_t = SelfBuildPromoteMode::None)]
        promote_mode: SelfBuildPromoteMode,
        /// Base branch for PR creation/merge flow
        #[arg(long, default_value = "main")]
        base_branch: String,
        /// Monitor CI after opening a PR
        #[arg(long)]
        monitor_ci: bool,
        /// Explicitly disable CI monitoring after opening a PR
        #[arg(long, conflicts_with = "monitor_ci")]
        no_monitor_ci: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SelfBuildPromoteMode {
    None,
    Pr,
    Auto,
}

fn format_epoch(epoch_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".into())
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

    match dotenvy::dotenv_override() {
        Ok(path) => eprintln!("Loaded .env from {}", path.display()),
        Err(e) if e.not_found() => {
            // No .env file — fine, rely on process environment
        }
        Err(e) => eprintln!("Warning: failed to load .env: {}", e),
    }

    if let Err(e) = secrets::load_keyring_into_env() {
        eprintln!("Warning: failed to load keyring secrets: {}", e);
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
            // List merged ghost definitions without requiring LLM connectivity.
            let ghosts = profiles::load_ghosts(&config)?;
            for g in ghosts {
                println!("  {} — {} [{}]", g.name, g.description, g.tools.join(", "));
            }
        }
        Some(Commands::Secrets { action }) => handle_secrets(action)?,
        #[cfg(feature = "telegram")]
        Some(Commands::Telegram) => {
            let mut telegram_config = config.clone();
            let telegram_provider = telegram_config
                .telegram
                .provider
                .clone()
                .unwrap_or_else(|| "ouath".into());
            telegram_config.llm.provider = telegram_provider.clone();
            let ouath_cfg = telegram_config.ouath.clone().unwrap_or_default();

            let system_info = telegram::SystemInfo {
                provider: telegram_provider.clone(),
                temperature: match telegram_provider.as_str() {
                    "ouath" => ouath_cfg.temperature,
                    "openrouter" => config
                        .openrouter
                        .as_ref()
                        .map(|c| c.temperature)
                        .unwrap_or(0.3),
                    "zen" => config.zen.as_ref().map(|c| c.temperature).unwrap_or(0.3),
                    _ => config.ollama.temperature,
                },
                max_tokens: match telegram_provider.as_str() {
                    "ouath" => ouath_cfg.max_tokens,
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
            let handle = AthenaCore::start(telegram_config.clone(), memory).await?;
            telegram::run_telegram(handle, telegram_config.telegram, system_info).await?;
        }
        Some(Commands::Ouath { action }) => {
            let ouath_config = config.ouath.clone().unwrap_or_default();
            let auth = ouath::OuathAuth::new(ouath_config);
            match action {
                OuathAction::Login => {
                    let tokens = auth.login_interactive().await?;
                    let account = tokens.chatgpt_account_id.as_deref().unwrap_or("unknown");
                    let expires = format_epoch(tokens.expires_at);
                    println!("Ouath login complete.");
                    println!("  Account: {}", account);
                    println!("  Expires: {}", expires);
                }
                OuathAction::Status => match auth.load_tokens().await? {
                    Some(tokens) => {
                        let account = tokens.chatgpt_account_id.as_deref().unwrap_or("unknown");
                        let expires = format_epoch(tokens.expires_at);
                        println!("Ouath tokens found.");
                        println!("  Account: {}", account);
                        println!("  Expires: {}", expires);
                        if tokens.expired(60) {
                            println!("  Status: expired (refresh on next use)");
                        } else {
                            println!("  Status: valid");
                        }
                    }
                    None => {
                        println!("No Ouath tokens found. Run `athena ouath login`.");
                    }
                },
                OuathAction::Logout => {
                    auth.logout().await?;
                    println!("Ouath tokens removed.");
                }
            }
        }
        Some(Commands::Observe) => unreachable!(), // handled above
        Some(Commands::CiMonitor {
            pr,
            branch,
            auto_merge,
            heal,
            max_heal,
            poll_interval,
            timeout,
        }) => {
            run_ci_monitor(
                config,
                pr,
                branch,
                auto_merge,
                heal,
                max_heal,
                poll_interval,
                timeout,
            )
            .await?
        }
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
        Some(Commands::SelfBuild { action }) => handle_self_build(action, config, memory).await?,
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

fn handle_secrets(action: SecretsAction) -> anyhow::Result<()> {
    match action {
        SecretsAction::List => {
            let mut report = secrets::keyring_report();
            report.statuses.sort_by(|a, b| a.key.cmp(b.key));
            for status in report.statuses {
                let env = if status.in_env { "set" } else { "unset" };
                let keyring = if status.in_keyring { "set" } else { "unset" };
                println!(
                    "{:<24} env={} keyring={}",
                    status.key, env, keyring
                );
            }
            if let Some(err) = report.error {
                println!("keyring_error={}", err);
            }
        }
        SecretsAction::Set { key } => {
            secrets::set_secret(&key)?;
            println!("secret_saved={}", key);
        }
        SecretsAction::Delete { key } => {
            secrets::delete_secret(&key)?;
            println!("secret_deleted={}", key);
        }
    }
    Ok(())
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
struct CommandRunResult {
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildOutcomeRecord {
    status: Option<String>,
    error: Option<String>,
    verification_total: Option<u64>,
    verification_passed: Option<u64>,
    rolled_back: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildDispatchSummary {
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    attempts: u8,
    noop_retry_used: bool,
    status: String,
    task_id: Option<String>,
    outcome: Option<SelfBuildOutcomeRecord>,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildMaintenanceStep {
    name: String,
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    status: String,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildGuardrailViolation {
    code: String,
    message: String,
    hard_block: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildGuardrailReport {
    passed: bool,
    hard_blocked: bool,
    hard_block_codes: Vec<String>,
    details: Vec<SelfBuildGuardrailViolation>,
    violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildCriticReport {
    score: f64,
    passed: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildPromotionDecision {
    auto_promote_recommended: bool,
    approval_required: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildPromotionCommand {
    name: String,
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    status: String,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildPromotionExecution {
    mode: String,
    status: String,
    branch: Option<String>,
    commit: Option<String>,
    pr_url: Option<String>,
    merged: bool,
    reasons: Vec<String>,
    commands: Vec<SelfBuildPromotionCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildReviewChecklist {
    timestamp_utc: String,
    run_id: String,
    risk_tier: String,
    ticket: String,
    change_files_count: usize,
    blast_radius_summary: String,
    rollback_plan: Vec<String>,
    evidence: Vec<String>,
    blockers: Vec<String>,
    merge_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfBuildLedger {
    timestamp_utc: String,
    run_id: String,
    ticket: String,
    context: Option<String>,
    risk_tier: String,
    wait_secs: u64,
    cli_tool: Option<String>,
    cli_model: Option<String>,
    maintenance_profile: String,
    allow_auto_promote: bool,
    promote_mode: String,
    base_branch: String,
    review_checklist_json: String,
    review_checklist_md: String,
    worktree_path: String,
    kept_worktree: bool,
    cleanup_error: Option<String>,
    changed_files: Vec<String>,
    diff_numstat: Vec<String>,
    dispatch: SelfBuildDispatchSummary,
    maintenance: Vec<SelfBuildMaintenanceStep>,
    guardrails: SelfBuildGuardrailReport,
    critic: SelfBuildCriticReport,
    review_checklist: SelfBuildReviewChecklist,
    promotion: SelfBuildPromotionDecision,
    promotion_execution: SelfBuildPromotionExecution,
    ci_monitor: Option<ci_monitor::CiMonitorReport>,
}

#[derive(Debug, Clone)]
struct MaintenanceCommandSpec {
    name: &'static str,
    program: &'static str,
    args: Vec<String>,
    timeout_secs: u64,
}

#[derive(Debug, Clone)]
struct SelfBuildPromotionPlan {
    status: &'static str,
    open_pr: bool,
    merge_pr: bool,
    reasons: Vec<String>,
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn self_build_promote_mode_label(mode: SelfBuildPromoteMode) -> &'static str {
    match mode {
        SelfBuildPromoteMode::None => "none",
        SelfBuildPromoteMode::Pr => "pr",
        SelfBuildPromoteMode::Auto => "auto",
    }
}

fn resolve_self_build_ci_monitor(
    promote_mode: SelfBuildPromoteMode,
    monitor_ci: bool,
    no_monitor_ci: bool,
) -> (bool, &'static str) {
    if monitor_ci {
        return (true, "explicit_on");
    }
    if no_monitor_ci {
        return (false, "explicit_off");
    }
    if promote_mode == SelfBuildPromoteMode::Auto {
        return (true, "auto_default");
    }
    (false, "default_off")
}

fn self_build_promotion_branch(run_id: &str) -> String {
    let safe = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("selfbuild/{}", safe)
}

fn trim_for_single_line(input: &str, max_chars: usize) -> String {
    let mut out = input.trim().replace('\n', " ");
    if out.chars().count() > max_chars {
        out = out
            .chars()
            .take(max_chars)
            .collect::<String>()
            .trim()
            .to_string();
    }
    out
}

fn self_build_commit_message(ticket: &str, run_id: &str) -> String {
    format!(
        "self-build: {} ({})",
        trim_for_single_line(ticket, 72),
        run_id
    )
}

fn self_build_pr_title(ticket: &str, run_id: &str) -> String {
    format!(
        "self-build: {} [{}]",
        trim_for_single_line(ticket, 60),
        run_id
    )
}

fn self_build_pr_body(run_id: &str, risk: &str, ticket: &str) -> String {
    format!(
        "Automated supervised self-build run.\n\n- run_id: `{}`\n- risk_tier: `{}`\n- ticket: {}\n\nGenerated by `athena self-build run`.",
        run_id,
        risk,
        trim_for_single_line(ticket, 160)
    )
}

fn parse_pr_url(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"https://github\.com/[^\s]+/pull/\d+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

fn build_promotion_command(name: &str, run: CommandRunResult) -> SelfBuildPromotionCommand {
    let status = if run.timed_out {
        "timeout"
    } else if run.exit_code == Some(0) {
        "passed"
    } else {
        "failed"
    };
    SelfBuildPromotionCommand {
        name: name.to_string(),
        command: run.command,
        exit_code: run.exit_code,
        timed_out: run.timed_out,
        duration_ms: run.duration_ms,
        status: status.to_string(),
        stdout_tail: tail_text(&run.stdout, 900),
        stderr_tail: tail_text(&run.stderr, 900),
    }
}

fn guardrail_violation(
    code: &str,
    message: impl Into<String>,
    hard_block: bool,
) -> SelfBuildGuardrailViolation {
    SelfBuildGuardrailViolation {
        code: code.to_string(),
        message: message.into(),
        hard_block,
    }
}

fn parse_numstat_totals(diff_numstat: &[String]) -> (u64, u64) {
    let mut added = 0u64;
    let mut deleted = 0u64;
    for line in diff_numstat {
        let mut parts = line.split_whitespace();
        let a = parts.next().unwrap_or("0");
        let d = parts.next().unwrap_or("0");
        if a != "-" {
            added = added.saturating_add(a.parse::<u64>().unwrap_or(0));
        }
        if d != "-" {
            deleted = deleted.saturating_add(d.parse::<u64>().unwrap_or(0));
        }
    }
    (added, deleted)
}

fn build_self_build_review_checklist(
    run_id: &str,
    risk_tier: &str,
    ticket: &str,
    changed_files: &[String],
    diff_numstat: &[String],
    guardrails: &SelfBuildGuardrailReport,
    critic: &SelfBuildCriticReport,
    maintenance: &[SelfBuildMaintenanceStep],
) -> SelfBuildReviewChecklist {
    let (added, deleted) = parse_numstat_totals(diff_numstat);
    let mut blockers = Vec::new();
    if guardrails.hard_blocked {
        blockers.push(format!(
            "hard guardrail policy blocks promotion: {}",
            guardrails.hard_block_codes.join(",")
        ));
    }
    if !critic.passed {
        blockers.push("critic did not pass quality gates".to_string());
    }
    if changed_files.is_empty() {
        blockers.push("no code changes were produced".to_string());
    }
    if maintenance.iter().any(|m| m.status != "passed") {
        blockers.push("maintenance pack reported failures".to_string());
    }
    let blast_radius = if changed_files.len() <= 3 && added + deleted <= 120 {
        "low"
    } else if changed_files.len() <= 8 && added + deleted <= 600 {
        "medium"
    } else {
        "high"
    };
    let blast_radius_summary = format!(
        "{} (files={}, added={}, deleted={})",
        blast_radius,
        changed_files.len(),
        added,
        deleted
    );

    let evidence = vec![
        format!(
            "dispatch_terminal_status={}",
            critic.reasons.is_empty() || critic.passed
        ),
        format!(
            "maintenance_all_passed={}",
            maintenance.iter().all(|m| m.status == "passed")
        ),
        format!("guardrails_passed={}", guardrails.passed),
        format!("critic_score={:.2}", critic.score),
    ];
    let rollback_plan = vec![
        "If PR is open and unmerged: close PR and delete remote self-build branch.".to_string(),
        "If merged: revert merge commit with `git revert <merge_commit_sha>` and run maintenance pack."
            .to_string(),
        "If regression persists: disable auto-promote for subsequent runs and require manual approval."
            .to_string(),
    ];

    SelfBuildReviewChecklist {
        timestamp_utc: chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        run_id: run_id.to_string(),
        risk_tier: risk_tier.to_string(),
        ticket: trim_for_single_line(ticket, 200),
        change_files_count: changed_files.len(),
        blast_radius_summary,
        rollback_plan,
        evidence,
        blockers: blockers.clone(),
        merge_ready: blockers.is_empty(),
    }
}

fn write_self_build_review_artifacts(
    base_repo: &Path,
    checklist: &SelfBuildReviewChecklist,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let out_dir = base_repo.join("eval").join("results");
    std::fs::create_dir_all(&out_dir)?;
    let base = format!("self-build-review-{}", checklist.run_id);
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    let latest_json = out_dir.join("self-build-review-latest.json");
    let latest_md = out_dir.join("self-build-review-latest.md");
    std::fs::write(&json_path, serde_json::to_string_pretty(checklist)?)?;
    std::fs::write(&md_path, render_self_build_review_markdown(checklist))?;
    let _ = std::fs::copy(&json_path, &latest_json);
    let _ = std::fs::copy(&md_path, &latest_md);
    Ok((json_path, md_path))
}

fn render_self_build_review_markdown(checklist: &SelfBuildReviewChecklist) -> String {
    let mut out = String::new();
    out.push_str("# Self-Build Review Checklist\n\n");
    out.push_str(&format!("- run_id: `{}`\n", checklist.run_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", checklist.timestamp_utc));
    out.push_str(&format!("- risk_tier: `{}`\n", checklist.risk_tier));
    out.push_str(&format!("- merge_ready: `{}`\n", checklist.merge_ready));
    out.push_str(&format!(
        "- blast_radius: `{}`\n",
        checklist.blast_radius_summary
    ));
    out.push_str(&format!("- ticket: {}\n\n", checklist.ticket));

    out.push_str("## Evidence\n\n");
    for ev in &checklist.evidence {
        out.push_str(&format!("- {}\n", ev));
    }
    out.push('\n');

    out.push_str("## Rollback Plan\n\n");
    for step in &checklist.rollback_plan {
        out.push_str(&format!("- {}\n", step));
    }
    out.push('\n');

    out.push_str("## Blockers\n\n");
    if checklist.blockers.is_empty() {
        out.push_str("- none\n");
    } else {
        for b in &checklist.blockers {
            out.push_str(&format!("- {}\n", b));
        }
    }
    out
}

fn plan_self_build_promotion(
    mode: SelfBuildPromoteMode,
    risk_tier: &str,
    auto_promote_recommended: bool,
    critic_passed: bool,
    has_changes: bool,
    guardrails: &SelfBuildGuardrailReport,
    review_merge_ready: bool,
) -> SelfBuildPromotionPlan {
    if mode == SelfBuildPromoteMode::None {
        return SelfBuildPromotionPlan {
            status: "skipped",
            open_pr: false,
            merge_pr: false,
            reasons: vec!["promote_mode=none".to_string()],
        };
    }
    if guardrails.hard_blocked {
        return SelfBuildPromotionPlan {
            status: "blocked",
            open_pr: false,
            merge_pr: false,
            reasons: guardrails
                .hard_block_codes
                .iter()
                .map(|code| format!("policy.guardrail.{}", code))
                .collect(),
        };
    }
    if !critic_passed {
        return SelfBuildPromotionPlan {
            status: "blocked",
            open_pr: false,
            merge_pr: false,
            reasons: vec!["critic did not pass quality gates".to_string()],
        };
    }
    if !has_changes {
        return SelfBuildPromotionPlan {
            status: "blocked",
            open_pr: false,
            merge_pr: false,
            reasons: vec!["no changes available to promote".to_string()],
        };
    }
    if mode == SelfBuildPromoteMode::Pr {
        return SelfBuildPromotionPlan {
            status: "ready_pr",
            open_pr: true,
            merge_pr: false,
            reasons: vec!["PR-only mode selected".to_string()],
        };
    }
    // auto mode
    if risk_tier != "low" {
        return SelfBuildPromotionPlan {
            status: "ready_pr",
            open_pr: true,
            merge_pr: false,
            reasons: vec![format!(
                "risk tier '{}' requires PR-only human approval",
                risk_tier
            )],
        };
    }
    if auto_promote_recommended {
        if !review_merge_ready {
            return SelfBuildPromotionPlan {
                status: "ready_pr",
                open_pr: true,
                merge_pr: false,
                reasons: vec!["review checklist is not merge-ready".to_string()],
            };
        }
        return SelfBuildPromotionPlan {
            status: "ready_merge",
            open_pr: true,
            merge_pr: true,
            reasons: vec!["low risk + auto-promote recommendation satisfied".to_string()],
        };
    }
    SelfBuildPromotionPlan {
        status: "ready_pr",
        open_pr: true,
        merge_pr: false,
        reasons: vec!["auto criteria not met; opening PR for human review".to_string()],
    }
}

fn self_build_policy_reason_codes(
    guardrails: &SelfBuildGuardrailReport,
    critic: &SelfBuildCriticReport,
    checklist: &SelfBuildReviewChecklist,
) -> Vec<String> {
    let mut out = Vec::new();
    if guardrails.hard_blocked {
        out.extend(
            guardrails
                .hard_block_codes
                .iter()
                .map(|c| format!("policy.guardrail.{}", c)),
        );
    }
    if !critic.passed {
        out.push("policy.critic_not_passed".to_string());
    }
    if !checklist.merge_ready {
        out.push("policy.review_not_merge_ready".to_string());
    }
    if out.is_empty() {
        out.push("policy.ok".to_string());
    }
    out.sort();
    out.dedup();
    out
}

fn command_succeeded(run: &CommandRunResult) -> bool {
    !run.timed_out && run.exit_code == Some(0)
}

fn command_combined_output(run: &CommandRunResult) -> String {
    if run.stderr.trim().is_empty() {
        run.stdout.clone()
    } else if run.stdout.trim().is_empty() {
        run.stderr.clone()
    } else {
        format!("{}\n{}", run.stdout, run.stderr)
    }
}

async fn run_command_capture(
    workdir: &Path,
    program: &str,
    args: &[String],
    timeout_secs: u64,
) -> CommandRunResult {
    use tokio::process::Command;

    let command_line = if args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, args.join(" "))
    };
    let start = std::time::Instant::now();
    let timed = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        Command::new(program)
            .args(args)
            .current_dir(workdir)
            .env("TERM", "dumb")
            .output(),
    )
    .await;

    let duration_ms = start.elapsed().as_millis() as u64;
    match timed {
        Ok(Ok(output)) => CommandRunResult {
            command: command_line,
            exit_code: output.status.code(),
            timed_out: false,
            duration_ms,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Ok(Err(e)) => CommandRunResult {
            command: command_line,
            exit_code: None,
            timed_out: false,
            duration_ms,
            stdout: String::new(),
            stderr: e.to_string(),
        },
        Err(_) => CommandRunResult {
            command: command_line,
            exit_code: None,
            timed_out: true,
            duration_ms,
            stdout: String::new(),
            stderr: format!("timed out after {}s", timeout_secs),
        },
    }
}

async fn resolve_repo_root() -> anyhow::Result<PathBuf> {
    let out = run_command_capture(
        Path::new("."),
        "git",
        &args(&["rev-parse", "--show-toplevel"]),
        30,
    )
    .await;
    if !command_succeeded(&out) {
        anyhow::bail!(
            "Failed to resolve git repo root: {}",
            tail_text(&command_combined_output(&out), 400)
        );
    }
    let root = out.stdout.trim();
    if root.is_empty() {
        anyhow::bail!("Git reported empty repo root");
    }
    Ok(PathBuf::from(root))
}

fn parse_dispatch_task_id(text: &str) -> Option<String> {
    let re = regex::Regex::new(
        r"task_id=([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})",
    )
    .ok()?;
    let match_id = re
        .captures_iter(text)
        .next()
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    match_id
}

fn parse_git_status_paths(status_out: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in status_out.lines() {
        if line.trim().is_empty() || line.len() < 4 {
            continue;
        }
        let mut path = line[3..].trim().to_string();
        if let Some((_, right)) = path.split_once(" -> ") {
            path = right.trim().to_string();
        }
        if !path.is_empty() {
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn read_task_outcome_record(
    config: &Config,
    task_id: &str,
) -> anyhow::Result<Option<SelfBuildOutcomeRecord>> {
    use rusqlite::OptionalExtension;
    let conn = kpi::open_connection(config)?;
    let row = conn
        .query_row(
            "SELECT status, error, verification_total, verification_passed, rolled_back
             FROM autonomous_task_outcomes
             WHERE task_id = ?1",
            rusqlite::params![task_id],
            |r| {
                let status: String = r.get(0)?;
                let error: Option<String> = r.get(1)?;
                let vt: i64 = r.get(2)?;
                let vp: i64 = r.get(3)?;
                let rb: i64 = r.get(4)?;
                Ok(SelfBuildOutcomeRecord {
                    status: Some(status),
                    error,
                    verification_total: Some(vt.max(0) as u64),
                    verification_passed: Some(vp.max(0) as u64),
                    rolled_back: Some(rb != 0),
                })
            },
        )
        .optional()?;
    Ok(row)
}

async fn create_self_build_worktree(base_repo: &Path, run_id: &str) -> anyhow::Result<PathBuf> {
    let worktree_root = base_repo.join("eval").join(".selfbuild-worktrees");
    std::fs::create_dir_all(&worktree_root)?;
    let path = worktree_root.join(run_id);
    let path_s = path.to_string_lossy().to_string();
    let run = run_command_capture(
        base_repo,
        "git",
        &args(&["worktree", "add", "--detach", &path_s, "HEAD"]),
        120,
    )
    .await;
    if !command_succeeded(&run) {
        anyhow::bail!(
            "git worktree add failed: {}",
            tail_text(&command_combined_output(&run), 600)
        );
    }
    Ok(path)
}

fn resolve_child_dispatch_config_path(base_repo: &Path) -> Option<PathBuf> {
    let local = base_repo.join("config.toml");
    if local.exists() {
        return Some(local);
    }
    dirs::home_dir()
        .map(|h| h.join(".athena").join("config.toml"))
        .filter(|p| p.exists())
}

async fn remove_self_build_worktree(base_repo: &Path, worktree_path: &Path) -> Option<String> {
    let path_s = worktree_path.to_string_lossy().to_string();
    let remove = run_command_capture(
        base_repo,
        "git",
        &args(&["worktree", "remove", "--force", &path_s]),
        120,
    )
    .await;
    if !command_succeeded(&remove) {
        return Some(format!(
            "git worktree remove failed: {}",
            tail_text(&command_combined_output(&remove), 400)
        ));
    }
    let _ = run_command_capture(base_repo, "git", &args(&["worktree", "prune"]), 60).await;
    None
}

fn maintenance_command_specs(profile: &str) -> Vec<MaintenanceCommandSpec> {
    match profile {
        "rust" => vec![
            MaintenanceCommandSpec {
                name: "cargo_fmt_apply",
                program: "cargo",
                args: args(&["fmt", "--all"]),
                timeout_secs: 600,
            },
            MaintenanceCommandSpec {
                name: "cargo_fmt_check",
                program: "cargo",
                args: args(&["fmt", "--all", "--check"]),
                timeout_secs: 600,
            },
            MaintenanceCommandSpec {
                name: "cargo_check",
                program: "cargo",
                args: args(&["check"]),
                timeout_secs: 900,
            },
            MaintenanceCommandSpec {
                name: "cargo_test_workspace",
                program: "cargo",
                args: args(&["test", "--workspace", "--quiet"]),
                timeout_secs: 1800,
            },
        ],
        _ => vec![
            MaintenanceCommandSpec {
                name: "git_diff_check",
                program: "git",
                args: args(&["diff", "--check"]),
                timeout_secs: 120,
            },
            MaintenanceCommandSpec {
                name: "git_status_short",
                program: "git",
                args: args(&["status", "--short"]),
                timeout_secs: 120,
            },
        ],
    }
}

fn evaluate_self_build_guardrails(
    changed_files: &[String],
    diff_text: &str,
    dispatch_output: &str,
    worktree_branch: &str,
) -> SelfBuildGuardrailReport {
    let mut details = Vec::new();
    if !worktree_branch.trim().is_empty() {
        details.push(guardrail_violation(
            "worktree_not_detached",
            format!(
                "worktree attached to branch '{}' (expected detached HEAD)",
                worktree_branch.trim()
            ),
            true,
        ));
    }

    let destructive_patterns = [
        "git reset --hard",
        "git checkout --",
        "git clean -fd",
        "rm -rf /",
        "rm -rf *",
    ];
    let lowered_output = dispatch_output.to_lowercase();
    for pattern in destructive_patterns {
        if lowered_output.contains(pattern) {
            details.push(guardrail_violation(
                "destructive_git_or_shell",
                format!("detected destructive command pattern '{}'", pattern),
                true,
            ));
        }
    }

    for path in changed_files {
        let lower = path.to_lowercase();
        if lower.ends_with(".env")
            || lower.contains("secret")
            || lower.contains("credentials")
            || lower.contains("token")
        {
            details.push(guardrail_violation(
                "sensitive_file_changed",
                format!("sensitive-looking file changed in self-build run: {}", path),
                true,
            ));
        }
    }

    let added_lines: String = diff_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .collect::<Vec<_>>()
        .join("\n");
    let secret_assignment = regex::Regex::new(
        r#"(?i)(api[_-]?key|token|secret|password)\s*[:=]\s*["'][^"'\n]{8,}["']"#,
    )
    .unwrap();
    let token_like =
        regex::Regex::new(r#"(?i)\b(?:ghp_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9]{20,})\b"#).unwrap();
    if secret_assignment.is_match(&added_lines) || token_like.is_match(&added_lines) {
        details.push(guardrail_violation(
            "secret_like_diff",
            "detected secret-like material in added diff lines",
            true,
        ));
    }

    details.sort_by(|a, b| a.code.cmp(&b.code).then(a.message.cmp(&b.message)));
    details.dedup_by(|a, b| a.code == b.code && a.message == b.message);
    let violations = details
        .iter()
        .map(|v| v.message.clone())
        .collect::<Vec<_>>();
    let hard_block_codes = details
        .iter()
        .filter(|v| v.hard_block)
        .map(|v| v.code.clone())
        .collect::<Vec<_>>();
    SelfBuildGuardrailReport {
        passed: details.is_empty(),
        hard_blocked: !hard_block_codes.is_empty(),
        hard_block_codes,
        details,
        violations,
    }
}

fn evaluate_self_build_critic(
    dispatch_status: &str,
    dispatch_exit_code: Option<i32>,
    changed_files: &[String],
    maintenance: &[SelfBuildMaintenanceStep],
    guardrails: &SelfBuildGuardrailReport,
) -> SelfBuildCriticReport {
    let mut score = 0.0;
    let mut reasons = Vec::new();
    let dispatch_ok = dispatch_status == "succeeded" && dispatch_exit_code == Some(0);
    if dispatch_ok {
        score += 0.35;
    } else {
        reasons.push(format!("dispatch status is '{}'", dispatch_status));
    }

    if !changed_files.is_empty() {
        score += 0.10;
    } else {
        reasons.push("no repo changes detected from self-build dispatch".to_string());
    }

    let maintenance_ok = maintenance.iter().all(|m| m.status == "passed");
    if maintenance_ok {
        score += 0.35;
    } else {
        reasons.push("maintenance pack reported one or more failures".to_string());
    }

    if guardrails.passed {
        score += 0.20;
    } else {
        reasons.push("guardrail violations detected".to_string());
    }

    if score > 1.0 {
        score = 1.0;
    }
    let passed = dispatch_ok && maintenance_ok && guardrails.passed && !changed_files.is_empty();
    SelfBuildCriticReport {
        score,
        passed,
        reasons,
    }
}

fn decide_self_build_promotion(
    risk_tier: &str,
    allow_auto_promote: bool,
    critic: &SelfBuildCriticReport,
) -> SelfBuildPromotionDecision {
    let approval_required = risk_tier != "low";
    let mut reasons = Vec::new();
    let mut auto = false;

    if approval_required {
        reasons.push(format!("risk tier '{}' requires human approval", risk_tier));
    }
    if !allow_auto_promote {
        reasons.push("auto-promote disabled by CLI flag".to_string());
    }
    if !critic.passed {
        reasons.push("critic did not pass run quality checks".to_string());
    }
    if critic.score < 0.85 {
        reasons.push(format!(
            "critic score {:.2} is below auto-promote threshold 0.85",
            critic.score
        ));
    }

    if !approval_required && allow_auto_promote && critic.passed && critic.score >= 0.85 {
        auto = true;
    }

    SelfBuildPromotionDecision {
        auto_promote_recommended: auto,
        approval_required,
        reasons,
    }
}

async fn execute_self_build_promotion(
    worktree: &Path,
    mode: SelfBuildPromoteMode,
    base_branch: &str,
    run_id: &str,
    ticket: &str,
    risk_tier: &str,
    guardrails: &SelfBuildGuardrailReport,
    review_checklist: &SelfBuildReviewChecklist,
    decision: &SelfBuildPromotionDecision,
    critic: &SelfBuildCriticReport,
    changed_files: &[String],
) -> SelfBuildPromotionExecution {
    let plan = plan_self_build_promotion(
        mode,
        risk_tier,
        decision.auto_promote_recommended,
        critic.passed,
        !changed_files.is_empty(),
        guardrails,
        review_checklist.merge_ready,
    );
    let mut execution = SelfBuildPromotionExecution {
        mode: self_build_promote_mode_label(mode).to_string(),
        status: plan.status.to_string(),
        branch: None,
        commit: None,
        pr_url: None,
        merged: false,
        reasons: plan.reasons.clone(),
        commands: Vec::new(),
    };

    if !plan.open_pr {
        return execution;
    }

    let branch = self_build_promotion_branch(run_id);
    execution.branch = Some(branch.clone());

    let checkout =
        run_command_capture(worktree, "git", &args(&["checkout", "-b", &branch]), 60).await;
    execution
        .commands
        .push(build_promotion_command("checkout_branch", checkout.clone()));
    if !command_succeeded(&checkout) {
        execution.status = "failed".to_string();
        execution.reasons.push(format!(
            "failed to create branch '{}': {}",
            branch,
            tail_text(&command_combined_output(&checkout), 220)
        ));
        return execution;
    }

    let add_run = run_command_capture(worktree, "git", &args(&["add", "-A"]), 120).await;
    execution
        .commands
        .push(build_promotion_command("git_add", add_run.clone()));
    if !command_succeeded(&add_run) {
        execution.status = "failed".to_string();
        execution.reasons.push(format!(
            "git add failed: {}",
            tail_text(&command_combined_output(&add_run), 220)
        ));
        return execution;
    }

    let commit_msg = self_build_commit_message(ticket, run_id);
    let commit_run = run_command_capture(
        worktree,
        "git",
        &["commit".to_string(), "-m".to_string(), commit_msg],
        120,
    )
    .await;
    execution
        .commands
        .push(build_promotion_command("git_commit", commit_run.clone()));
    if !command_succeeded(&commit_run) {
        execution.status = "failed".to_string();
        execution.reasons.push(format!(
            "git commit failed: {}",
            tail_text(&command_combined_output(&commit_run), 220)
        ));
        return execution;
    }

    let rev = run_command_capture(worktree, "git", &args(&["rev-parse", "HEAD"]), 30).await;
    execution
        .commands
        .push(build_promotion_command("git_rev_parse", rev.clone()));
    if command_succeeded(&rev) {
        let hash = rev.stdout.trim();
        if !hash.is_empty() {
            execution.commit = Some(hash.to_string());
        }
    }

    let push_run = run_command_capture(
        worktree,
        "git",
        &args(&["push", "-u", "origin", &branch]),
        180,
    )
    .await;
    execution
        .commands
        .push(build_promotion_command("git_push", push_run.clone()));
    if !command_succeeded(&push_run) {
        execution.status = "failed".to_string();
        execution.reasons.push(format!(
            "git push failed: {}",
            tail_text(&command_combined_output(&push_run), 260)
        ));
        return execution;
    }

    let pr_title = self_build_pr_title(ticket, run_id);
    let pr_body = self_build_pr_body(run_id, risk_tier, ticket);
    let pr_run = run_command_capture(
        worktree,
        "gh",
        &[
            "pr".to_string(),
            "create".to_string(),
            "--base".to_string(),
            base_branch.to_string(),
            "--head".to_string(),
            branch.clone(),
            "--title".to_string(),
            pr_title,
            "--body".to_string(),
            pr_body,
        ],
        180,
    )
    .await;
    execution
        .commands
        .push(build_promotion_command("gh_pr_create", pr_run.clone()));
    if !command_succeeded(&pr_run) {
        execution.status = "failed".to_string();
        execution.reasons.push(format!(
            "gh pr create failed: {}",
            tail_text(&command_combined_output(&pr_run), 260)
        ));
        return execution;
    }

    let pr_out = command_combined_output(&pr_run);
    execution.pr_url = parse_pr_url(&pr_out).or_else(|| {
        let s = pr_run.stdout.trim();
        if s.starts_with("https://") {
            Some(s.to_string())
        } else {
            None
        }
    });
    execution.status = "pr_opened".to_string();

    if !plan.merge_pr {
        return execution;
    }

    let pr_target = execution.pr_url.clone().unwrap_or_else(|| branch.clone());
    let merge_run = run_command_capture(
        worktree,
        "gh",
        &[
            "pr".to_string(),
            "merge".to_string(),
            pr_target,
            "--squash".to_string(),
            "--delete-branch".to_string(),
        ],
        240,
    )
    .await;
    execution
        .commands
        .push(build_promotion_command("gh_pr_merge", merge_run.clone()));
    if command_succeeded(&merge_run) {
        execution.status = "merged".to_string();
        execution.merged = true;
    } else {
        execution.reasons.push(format!(
            "auto-merge failed; leaving PR open: {}",
            tail_text(&command_combined_output(&merge_run), 260)
        ));
    }

    execution
}

fn write_self_build_artifacts(
    base_repo: &Path,
    ledger: &SelfBuildLedger,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let out_dir = base_repo.join("eval").join("results");
    std::fs::create_dir_all(&out_dir)?;
    let safe_run_id = ledger
        .run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let base = format!("self-build-{}", safe_run_id);
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    let latest_json = out_dir.join("self-build-latest.json");
    let latest_md = out_dir.join("self-build-latest.md");
    std::fs::write(&json_path, serde_json::to_string_pretty(ledger)?)?;
    std::fs::write(&md_path, render_self_build_markdown(ledger))?;
    let _ = std::fs::copy(&json_path, &latest_json);
    let _ = std::fs::copy(&md_path, &latest_md);
    Ok((json_path, md_path))
}

fn render_self_build_md_header(out: &mut String, ledger: &SelfBuildLedger) {
    out.push_str("# Self-Build Run Ledger\n\n");
    out.push_str(&format!("- run_id: `{}`\n", ledger.run_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", ledger.timestamp_utc));
    out.push_str(&format!("- risk_tier: `{}`\n", ledger.risk_tier));
    out.push_str(&format!(
        "- worktree_path: `{}` (kept: `{}`)\n",
        ledger.worktree_path, ledger.kept_worktree
    ));
    out.push_str(&format!(
        "- maintenance_profile: `{}`\n",
        ledger.maintenance_profile
    ));
    out.push_str(&format!(
        "- allow_auto_promote: `{}`\n",
        ledger.allow_auto_promote
    ));
    out.push_str(&format!("- promote_mode: `{}`\n", ledger.promote_mode));
    out.push_str(&format!("- base_branch: `{}`\n", ledger.base_branch));
    out.push_str(&format!(
        "- review_checklist_json: `{}`\n",
        ledger.review_checklist_json
    ));
    out.push_str(&format!(
        "- review_checklist_md: `{}`\n",
        ledger.review_checklist_md
    ));
    out.push_str(&format!("- ticket: {}\n", ledger.ticket));
    if let Some(ctx) = &ledger.context {
        out.push_str(&format!("- context: {}\n", ctx));
    }
    if let Some(err) = &ledger.cleanup_error {
        out.push_str(&format!("- cleanup_error: {}\n", err));
    }
    out.push('\n');
}

fn render_self_build_md_dispatch(out: &mut String, dispatch: &SelfBuildDispatchSummary) {
    out.push_str("## Dispatch\n\n");
    out.push_str(&format!("- status: `{}`\n", dispatch.status));
    out.push_str(&format!("- command: `{}`\n", dispatch.command));
    out.push_str(&format!(
        "- exit_code: `{}` timed_out: `{}` duration_ms: `{}`\n",
        dispatch
            .exit_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string()),
        dispatch.timed_out,
        dispatch.duration_ms
    ));
    out.push_str(&format!(
        "- attempts: `{}` noop_retry_used: `{}`\n",
        dispatch.attempts, dispatch.noop_retry_used
    ));
    out.push_str(&format!(
        "- task_id: `{}`\n",
        dispatch.task_id.as_deref().unwrap_or("-")
    ));
    if let Some(outcome) = &dispatch.outcome {
        out.push_str(&format!(
            "- outcome_status: `{}` error: `{}` verify: `{}/{}` rolled_back: `{}`\n",
            outcome.status.as_deref().unwrap_or("-"),
            outcome.error.as_deref().unwrap_or("-"),
            outcome.verification_passed.unwrap_or(0),
            outcome.verification_total.unwrap_or(0),
            outcome.rolled_back.unwrap_or(false)
        ));
    }
    out.push('\n');
}

fn render_self_build_md_maintenance(out: &mut String, maintenance: &[SelfBuildMaintenanceStep]) {
    out.push_str("## Maintenance\n\n");
    out.push_str("| step | status | exit_code | timed_out | duration_ms |\n");
    out.push_str("|---|---|---|---|---|\n");
    for m in maintenance {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            m.name,
            m.status,
            m.exit_code
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            m.timed_out,
            m.duration_ms
        ));
    }
    out.push('\n');
}

fn render_self_build_md_guardrails(out: &mut String, guardrails: &SelfBuildGuardrailReport) {
    out.push_str("## Guardrails\n\n");
    out.push_str(&format!("- passed: `{}`\n", guardrails.passed));
    out.push_str(&format!(
        "- hard_blocked: `{}` codes: `{}`\n",
        guardrails.hard_blocked,
        if guardrails.hard_block_codes.is_empty() {
            "-".to_string()
        } else {
            guardrails.hard_block_codes.join(",")
        }
    ));
    if !guardrails.details.is_empty() {
        out.push_str("- details:\n");
        for d in &guardrails.details {
            out.push_str(&format!(
                "  - code={} hard_block={} message={}\n",
                d.code, d.hard_block, d.message
            ));
        }
    }
    if !guardrails.violations.is_empty() {
        out.push_str("- violations:\n");
        for v in &guardrails.violations {
            out.push_str(&format!("  - {}\n", v));
        }
    }
    out.push('\n');
}

fn render_self_build_md_review(out: &mut String, ledger: &SelfBuildLedger) {
    out.push_str("## Critic\n\n");
    out.push_str(&format!("- score: `{:.2}`\n", ledger.critic.score));
    out.push_str(&format!("- passed: `{}`\n", ledger.critic.passed));
    if !ledger.critic.reasons.is_empty() {
        out.push_str("- reasons:\n");
        for r in &ledger.critic.reasons {
            out.push_str(&format!("  - {}\n", r));
        }
    }
    out.push('\n');

    out.push_str("## Promotion Decision\n\n");
    out.push_str(&format!(
        "- auto_promote_recommended: `{}`\n",
        ledger.promotion.auto_promote_recommended
    ));
    out.push_str(&format!(
        "- approval_required: `{}`\n",
        ledger.promotion.approval_required
    ));
    if !ledger.promotion.reasons.is_empty() {
        out.push_str("- reasons:\n");
        for r in &ledger.promotion.reasons {
            out.push_str(&format!("  - {}\n", r));
        }
    }
    out.push('\n');

    out.push_str("## Review Checklist\n\n");
    out.push_str(&format!(
        "- merge_ready: `{}`\n",
        ledger.review_checklist.merge_ready
    ));
    out.push_str(&format!(
        "- blast_radius: `{}`\n",
        ledger.review_checklist.blast_radius_summary
    ));
    out.push_str("- rollback_plan:\n");
    for step in &ledger.review_checklist.rollback_plan {
        out.push_str(&format!("  - {}\n", step));
    }
    if !ledger.review_checklist.blockers.is_empty() {
        out.push_str("- blockers:\n");
        for b in &ledger.review_checklist.blockers {
            out.push_str(&format!("  - {}\n", b));
        }
    }
    out.push('\n');
}

fn render_self_build_md_promotion_exec(out: &mut String, exec: &SelfBuildPromotionExecution) {
    out.push_str("## Promotion Execution\n\n");
    out.push_str(&format!(
        "- mode: `{}` status: `{}` merged: `{}`\n",
        exec.mode, exec.status, exec.merged
    ));
    if let Some(branch) = &exec.branch {
        out.push_str(&format!("- branch: `{}`\n", branch));
    }
    if let Some(commit) = &exec.commit {
        out.push_str(&format!("- commit: `{}`\n", commit));
    }
    if let Some(pr) = &exec.pr_url {
        out.push_str(&format!("- pr_url: `{}`\n", pr));
    }
    if !exec.reasons.is_empty() {
        out.push_str("- reasons:\n");
        for r in &exec.reasons {
            out.push_str(&format!("  - {}\n", r));
        }
    }
    if !exec.commands.is_empty() {
        out.push_str("- commands:\n");
        for c in &exec.commands {
            out.push_str(&format!(
                "  - {} status={} exit={} timed_out={} duration_ms={}\n",
                c.name,
                c.status,
                c.exit_code
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                c.timed_out,
                c.duration_ms
            ));
        }
    }
    out.push('\n');
}

fn render_self_build_md_ci_monitor(out: &mut String, monitor: &ci_monitor::CiMonitorReport) {
    out.push_str("## CI Monitor\n\n");
    out.push_str(&format!("- pr_url: `{}`\n", monitor.pr_url));
    if let Some(branch) = &monitor.branch {
        out.push_str(&format!("- branch: `{}`\n", branch));
    }
    out.push_str(&format!(
        "- final_status: `{}` merged_after_ci: `{}`\n",
        monitor.final_status, monitor.merged_after_ci
    ));
    out.push_str(&format!(
        "- post_merge_status: `{}`\n",
        monitor.post_merge_status
    ));
    if let Some(url) = &monitor.revert_pr_url {
        out.push_str(&format!("- revert_pr_url: `{}`\n", url));
    }
    out.push_str(&format!(
        "- started_utc: `{}` finished_utc: `{}`\n",
        monitor.started_utc, monitor.finished_utc
    ));
    out.push_str(&format!(
        "- polls: `{}` heal_attempts: `{}`\n",
        monitor.polls.len(),
        monitor.heal_attempts.len()
    ));
    if let Some(last) = monitor.polls.last() {
        out.push_str(&format!(
            "- last_overall: `{}` at `{}`\n",
            last.overall, last.timestamp_utc
        ));
        if !last.checks.is_empty() {
            out.push_str("- last_checks:\n");
            for c in &last.checks {
                out.push_str(&format!(
                    "  - {} status={} conclusion={}\n",
                    c.name, c.status, c.conclusion
                ));
            }
        }
    }
    if !monitor.heal_attempts.is_empty() {
        out.push_str("- heal_attempts:\n");
        for h in &monitor.heal_attempts {
            out.push_str(&format!(
                "  - attempt={} status={} commit={}\n",
                h.attempt,
                h.dispatch_status,
                h.commit_sha.as_deref().unwrap_or("-")
            ));
        }
    }
    out.push('\n');
}

fn render_self_build_md_diff(out: &mut String, changed_files: &[String], diff_numstat: &[String]) {
    out.push_str("## Diff Summary\n\n");
    out.push_str(&format!("- changed_files: `{}`\n", changed_files.len()));
    if !changed_files.is_empty() {
        for p in changed_files {
            out.push_str(&format!("  - `{}`\n", p));
        }
    }
    if !diff_numstat.is_empty() {
        out.push_str("- numstat:\n");
        for line in diff_numstat {
            out.push_str(&format!("  - `{}`\n", line));
        }
    }
}

fn render_self_build_markdown(ledger: &SelfBuildLedger) -> String {
    let mut out = String::new();
    render_self_build_md_header(&mut out, ledger);
    render_self_build_md_dispatch(&mut out, &ledger.dispatch);
    render_self_build_md_maintenance(&mut out, &ledger.maintenance);
    render_self_build_md_guardrails(&mut out, &ledger.guardrails);
    render_self_build_md_review(&mut out, ledger);
    render_self_build_md_promotion_exec(&mut out, &ledger.promotion_execution);
    if let Some(ci) = &ledger.ci_monitor {
        render_self_build_md_ci_monitor(&mut out, ci);
    }
    render_self_build_md_diff(&mut out, &ledger.changed_files, &ledger.diff_numstat);
    out
}

async fn handle_self_build(
    action: SelfBuildAction,
    config: Config,
    memory: Arc<MemoryStore>,
) -> anyhow::Result<()> {
    match action {
        SelfBuildAction::Run {
            ticket,
            context,
            risk,
            wait_secs,
            cli_tool,
            cli_model,
            maintenance_profile,
            keep_worktree,
            allow_auto_promote,
            promote_mode,
            base_branch,
            monitor_ci,
            no_monitor_ci,
        } => {
            run_self_build(
                config,
                memory,
                ticket,
                context,
                risk,
                wait_secs,
                cli_tool,
                cli_model,
                maintenance_profile,
                keep_worktree,
                allow_auto_promote,
                promote_mode,
                base_branch,
                monitor_ci,
                no_monitor_ci,
            )
            .await
        }
    }
}

async fn run_ci_monitor(
    config: Config,
    pr: String,
    branch: Option<String>,
    auto_merge: bool,
    heal: bool,
    max_heal: u8,
    poll_interval: u64,
    timeout: u64,
) -> anyhow::Result<()> {
    let base_repo = resolve_repo_root().await?;
    let report = ci_monitor::monitor_pr_ci(
        &pr,
        branch.as_deref(),
        &base_repo,
        &config,
        auto_merge,
        heal,
        poll_interval,
        timeout,
        max_heal,
    )
    .await;
    let json_path = write_ci_monitor_artifact(&base_repo, &report)?;
    println!("ci_monitor_report={}", json_path.display());
    println!("ci_monitor_status={}", report.final_status);
    println!("ci_monitor_merged={}", report.merged_after_ci);
    println!("ci_monitor_post_merge_status={}", report.post_merge_status);
    if let Some(url) = &report.revert_pr_url {
        println!("ci_monitor_revert_pr_url={}", url);
    }
    println!("ci_monitor_polls={}", report.polls.len());
    println!("ci_monitor_heal_attempts={}", report.heal_attempts.len());
    Ok(())
}

fn write_ci_monitor_artifact(
    base_repo: &Path,
    report: &ci_monitor::CiMonitorReport,
) -> anyhow::Result<PathBuf> {
    let out_dir = base_repo.join("eval").join("results");
    std::fs::create_dir_all(&out_dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_label = sanitize_ci_monitor_label(&report.pr_url, 40);
    let base = format!("ci-monitor-{}-{}", stamp, safe_label);
    let json_path = out_dir.join(format!("{}.json", base));
    let latest_json = out_dir.join("ci-monitor-latest.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(report)?)?;
    let _ = std::fs::copy(&json_path, &latest_json);
    Ok(json_path)
}

fn sanitize_ci_monitor_label(input: &str, max_len: usize) -> String {
    let mut out = input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "pr".to_string();
    }
    if out.chars().count() > max_len {
        out = out.chars().take(max_len).collect();
    }
    out
}

fn resolve_dispatch_status(
    run: &CommandRunResult,
    outcome: Option<&SelfBuildOutcomeRecord>,
) -> String {
    if run.timed_out {
        "timeout".to_string()
    } else if let Some(status) = outcome.and_then(|o| o.status.clone()) {
        status
    } else if run.exit_code == Some(0) {
        "contract_error".to_string()
    } else {
        "failed".to_string()
    }
}

async fn collect_worktree_changed_files(worktree: &Path) -> Vec<String> {
    let status_run =
        run_command_capture(worktree, "git", &args(&["status", "--porcelain"]), 60).await;
    if command_succeeded(&status_run) {
        parse_git_status_paths(&status_run.stdout)
    } else {
        Vec::new()
    }
}

async fn collect_self_build_git_artifacts(
    worktree: &Path,
) -> (String, Vec<String>, String) {
    let diff_run =
        run_command_capture(worktree, "git", &args(&["diff", "--no-color"]), 120).await;
    let diff_text = command_combined_output(&diff_run);
    let numstat_run =
        run_command_capture(worktree, "git", &args(&["diff", "--numstat"]), 60).await;
    let diff_numstat = if command_succeeded(&numstat_run) {
        numstat_run
            .stdout
            .lines()
            .take(100)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let branch_run =
        run_command_capture(worktree, "git", &args(&["branch", "--show-current"]), 30).await;
    let branch_name = if command_succeeded(&branch_run) {
        branch_run.stdout.trim().to_string()
    } else {
        String::new()
    };
    (diff_text, diff_numstat, branch_name)
}

async fn run_self_build_maintenance(
    worktree: &Path,
    profile: &str,
) -> Vec<SelfBuildMaintenanceStep> {
    let specs = maintenance_command_specs(profile);
    let mut rows = Vec::new();
    for spec in specs {
        let run =
            run_command_capture(worktree, spec.program, &spec.args, spec.timeout_secs).await;
        let status = if run.timed_out {
            "timeout"
        } else if run.exit_code == Some(0) {
            "passed"
        } else {
            "failed"
        };
        rows.push(SelfBuildMaintenanceStep {
            name: spec.name.to_string(),
            command: run.command,
            exit_code: run.exit_code,
            timed_out: run.timed_out,
            duration_ms: run.duration_ms,
            status: status.to_string(),
            stdout_tail: tail_text(&run.stdout, 900),
            stderr_tail: tail_text(&run.stderr, 900),
        });
    }
    rows
}

fn print_self_build_summary(
    ledger: &SelfBuildLedger,
    json_path: &Path,
    md_path: &Path,
    review_json_path: &Path,
    review_md_path: &Path,
) {
    println!("self_build_json={}", json_path.display());
    println!("self_build_md={}", md_path.display());
    println!("self_build_review_json={}", review_json_path.display());
    println!("self_build_review_md={}", review_md_path.display());
    println!("self_build_run_id={}", ledger.run_id);
    println!(
        "self_build_auto_promote_recommended={}",
        ledger.promotion.auto_promote_recommended
    );
    println!("self_build_guardrails_passed={}", ledger.guardrails.passed);
    println!("self_build_critic_score={:.2}", ledger.critic.score);
    println!(
        "self_build_promotion_status={}",
        ledger.promotion_execution.status
    );
    if let Some(url) = &ledger.promotion_execution.pr_url {
        println!("self_build_pr_url={}", url);
    }
    if let Some(ci) = &ledger.ci_monitor {
        println!("self_build_ci_monitor_status={}", ci.final_status);
        println!("self_build_ci_monitor_merged={}", ci.merged_after_ci);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_self_build(
    config: Config,
    memory: Arc<MemoryStore>,
    ticket: String,
    context: Option<String>,
    risk: String,
    wait_secs: u64,
    cli_tool: Option<String>,
    cli_model: Option<String>,
    maintenance_profile: String,
    keep_worktree: bool,
    allow_auto_promote: bool,
    promote_mode: SelfBuildPromoteMode,
    base_branch: String,
    monitor_ci: bool,
    no_monitor_ci: bool,
) -> anyhow::Result<()> {
    validate_risk(&risk)?;
    let timestamp_utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let run_id = format!(
        "{}-{}",
        timestamp_utc,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let base_repo = resolve_repo_root().await?;
    let worktree = create_self_build_worktree(&base_repo, &run_id).await?;
    let worktree_s = worktree.to_string_lossy().to_string();

    let repo_label = base_repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let exe = std::env::current_exe()?;
    let exe_s = exe.to_string_lossy().to_string();
    let mut dispatch_context = format!(
        "[self_build_run:{}]\nTicket: {}\nConstraints:\n- Follow repository guardrails.\n- Avoid destructive git operations.\n- Do not print or hardcode secrets.\n",
        run_id, ticket
    );
    if let Some(ctx) = context.as_deref() {
        dispatch_context.push_str("\nAdditional context:\n");
        dispatch_context.push_str(ctx);
    }
    let child_config_path = resolve_child_dispatch_config_path(&base_repo);
    let dispatch_timeout = wait_secs.saturating_add(180).max(240);
    let build_dispatch_args = |ctx: String| {
        let mut args = vec![
            "dispatch".to_string(),
            "--goal".to_string(),
            ticket.clone(),
            "--context".to_string(),
            ctx,
            "--ghost".to_string(),
            "coder".to_string(),
            "--auto-store".to_string(),
            "self_build_run".to_string(),
            "--wait-secs".to_string(),
            wait_secs.to_string(),
            "--lane".to_string(),
            "self_improvement".to_string(),
            "--risk".to_string(),
            risk.clone(),
            "--repo".to_string(),
            repo_label.clone(),
        ];
        if let Some(config_path) = child_config_path.as_ref() {
            args.insert(0, config_path.to_string_lossy().to_string());
            args.insert(0, "--config".to_string());
        }
        if let Some(tool) = cli_tool.as_deref() {
            args.push("--cli-tool".to_string());
            args.push(tool.to_string());
        }
        if let Some(model) = cli_model.as_deref() {
            args.push("--cli-model".to_string());
            args.push(model.to_string());
        }
        args
    };
    let mut dispatch_context_current = dispatch_context;
    let mut dispatch_args = build_dispatch_args(dispatch_context_current.clone());
    let mut dispatch_run =
        run_command_capture(&worktree, &exe_s, &dispatch_args, dispatch_timeout).await;
    let mut dispatch_output = command_combined_output(&dispatch_run);
    let mut dispatch_task_id = parse_dispatch_task_id(&dispatch_output);
    if let Some(task_id) = dispatch_task_id.as_deref() {
        let _ = wait_for_terminal_outcome_status(&config, task_id, 30).await?;
    }
    let mut dispatch_outcome = if let Some(task_id) = dispatch_task_id.as_deref() {
        read_task_outcome_record(&config, task_id)?
    } else {
        None
    };
    let mut dispatch_status =
        resolve_dispatch_status(&dispatch_run, dispatch_outcome.as_ref());
    let mut dispatch_attempts = 1u8;
    let mut noop_retry_used = false;
    let mut changed_files = collect_worktree_changed_files(&worktree).await;
    if dispatch_status == "succeeded"
        && dispatch_run.exit_code == Some(0)
        && changed_files.is_empty()
    {
        noop_retry_used = true;
        dispatch_attempts = 2;
        dispatch_context_current.push_str(
            "\nRetry directive:\nThe previous self-build attempt produced zero repository changes. Re-read the target file(s) from disk and apply the requested patch. Only report completion after a concrete git diff exists.\n",
        );
        dispatch_args = build_dispatch_args(dispatch_context_current.clone());
        dispatch_run =
            run_command_capture(&worktree, &exe_s, &dispatch_args, dispatch_timeout).await;
        dispatch_output = command_combined_output(&dispatch_run);
        dispatch_task_id = parse_dispatch_task_id(&dispatch_output);
        if let Some(task_id) = dispatch_task_id.as_deref() {
            let _ = wait_for_terminal_outcome_status(&config, task_id, 30).await?;
        }
        dispatch_outcome = if let Some(task_id) = dispatch_task_id.as_deref() {
            read_task_outcome_record(&config, task_id)?
        } else {
            None
        };
        dispatch_status =
            resolve_dispatch_status(&dispatch_run, dispatch_outcome.as_ref());
        changed_files = collect_worktree_changed_files(&worktree).await;
    }
    let dispatch_summary = SelfBuildDispatchSummary {
        command: dispatch_run.command.clone(),
        exit_code: dispatch_run.exit_code,
        timed_out: dispatch_run.timed_out,
        duration_ms: dispatch_run.duration_ms,
        attempts: dispatch_attempts,
        noop_retry_used,
        status: dispatch_status.clone(),
        task_id: dispatch_task_id.clone(),
        outcome: dispatch_outcome.clone(),
        stdout_tail: tail_text(&dispatch_run.stdout, 1200),
        stderr_tail: tail_text(&dispatch_run.stderr, 1200),
    };

    let (diff_text, diff_numstat, branch_name) =
        collect_self_build_git_artifacts(&worktree).await;
    let maintenance_rows =
        run_self_build_maintenance(&worktree, &maintenance_profile).await;

    let guardrails =
        evaluate_self_build_guardrails(&changed_files, &diff_text, &dispatch_output, &branch_name);
    let critic = evaluate_self_build_critic(
        &dispatch_status,
        dispatch_summary.exit_code,
        &changed_files,
        &maintenance_rows,
        &guardrails,
    );
    let review_checklist = build_self_build_review_checklist(
        &run_id,
        &risk,
        &ticket,
        &changed_files,
        &diff_numstat,
        &guardrails,
        &critic,
        &maintenance_rows,
    );
    let (review_json_path, review_md_path) =
        write_self_build_review_artifacts(&base_repo, &review_checklist)?;
    let promotion = decide_self_build_promotion(&risk, allow_auto_promote, &critic);
    let policy_reason_codes =
        self_build_policy_reason_codes(&guardrails, &critic, &review_checklist);
    let policy_promotion_allowed = !guardrails.hard_blocked
        && critic.passed
        && (promote_mode == SelfBuildPromoteMode::None
            || promote_mode == SelfBuildPromoteMode::Pr
            || review_checklist.merge_ready);
    println!(
        "self_build_policy promotion_allowed={} reason_codes={}",
        policy_promotion_allowed,
        policy_reason_codes.join(",")
    );
    let promotion_execution = execute_self_build_promotion(
        &worktree,
        promote_mode,
        &base_branch,
        &run_id,
        &ticket,
        &risk,
        &guardrails,
        &review_checklist,
        &promotion,
        &critic,
        &changed_files,
    )
    .await;

    let (monitor_ci_enabled, monitor_ci_source) =
        resolve_self_build_ci_monitor(promote_mode, monitor_ci, no_monitor_ci);
    if monitor_ci_source == "auto_default" {
        println!("auto-enabling CI monitor for promote_mode=auto");
    }

    let ci_monitor = if monitor_ci_enabled
        && promotion_execution.status == "pr_opened"
        && promotion_execution.pr_url.is_some()
    {
        let pr_url = promotion_execution.pr_url.as_ref().unwrap();
        Some(
            ci_monitor::monitor_pr_ci(
                pr_url,
                promotion_execution.branch.as_deref(),
                &base_repo,
                &config,
                promote_mode == SelfBuildPromoteMode::Auto,
                true,
                ci_monitor::CI_POLL_INTERVAL_SECS,
                ci_monitor::CI_POLL_TIMEOUT_SECS,
                ci_monitor::CI_HEAL_MAX_ATTEMPTS,
            )
            .await,
        )
    } else {
        None
    };

    let mut cleanup_error = None;
    if !keep_worktree {
        cleanup_error = remove_self_build_worktree(&base_repo, &worktree).await;
    }

    let ledger = SelfBuildLedger {
        timestamp_utc,
        run_id: run_id.clone(),
        ticket: ticket.clone(),
        context,
        risk_tier: risk,
        wait_secs,
        cli_tool,
        cli_model,
        maintenance_profile,
        allow_auto_promote,
        promote_mode: self_build_promote_mode_label(promote_mode).to_string(),
        base_branch: base_branch.clone(),
        review_checklist_json: review_json_path.display().to_string(),
        review_checklist_md: review_md_path.display().to_string(),
        worktree_path: worktree_s,
        kept_worktree: keep_worktree,
        cleanup_error: cleanup_error.clone(),
        changed_files,
        diff_numstat,
        dispatch: dispatch_summary,
        maintenance: maintenance_rows,
        guardrails,
        critic,
        review_checklist,
        promotion,
        promotion_execution,
        ci_monitor,
    };

    let (json_path, md_path) = write_self_build_artifacts(&base_repo, &ledger)?;
    print_self_build_summary(&ledger, &json_path, &md_path, &review_json_path, &review_md_path);

    let memory_category = if ledger.critic.passed {
        "self_build_run"
    } else {
        "self_build_failed"
    };
    let memory_summary = format!(
        "run_id={} dispatch_status={} guardrails_passed={} critic_passed={} score={:.2} auto_promote_recommended={} promotion_status={} pr_url={} artifacts={} {}",
        ledger.run_id,
        ledger.dispatch.status,
        ledger.guardrails.passed,
        ledger.critic.passed,
        ledger.critic.score,
        ledger.promotion.auto_promote_recommended,
        ledger.promotion_execution.status,
        ledger
            .promotion_execution
            .pr_url
            .as_deref()
            .unwrap_or("-"),
        json_path.display(),
        md_path.display()
    );
    let _ = memory.store(memory_category, &memory_summary, None);

    if !ledger.critic.passed {
        anyhow::bail!(
            "self-build run did not pass critic checks (see {})",
            md_path.display()
        );
    }
    if promote_mode != SelfBuildPromoteMode::None && ledger.promotion_execution.status == "failed" {
        anyhow::bail!(
            "self-build promotion execution failed (see {})",
            md_path.display()
        );
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureTaskLedgerRow {
    task_id: String,
    dispatch_task_id: Option<String>,
    status: String,
    reason: Option<String>,
    mapped_acceptance: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureAcceptanceLedgerRow {
    acceptance_id: String,
    covered_by_tasks: Vec<String>,
    succeeded_tasks: Vec<String>,
    covered: bool,
    satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureRunLedger {
    timestamp_utc: String,
    feature_id: String,
    contract_path: String,
    tasks: Vec<FeatureTaskLedgerRow>,
    acceptance: Vec<FeatureAcceptanceLedgerRow>,
    #[serde(default)]
    rollback_commits: Vec<FeatureRollbackLedgerRow>,
    summary: FeatureRunSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureRollbackLedgerRow {
    commit_sha: String,
    reverted: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureRunSummary {
    succeeded: usize,
    failed: usize,
    skipped: usize,
    acceptance_covered: bool,
    acceptance_satisfied: bool,
    promotable: bool,
    promotion_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureVerifyCheckRow {
    check_id: String,
    profile: String,
    command: String,
    required: bool,
    status: String,
    exit_code: Option<i32>,
    mapped_acceptance: Vec<String>,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureVerifyAcceptanceRow {
    acceptance_id: String,
    checks: Vec<String>,
    passed_checks: Vec<String>,
    satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureVerifyLedger {
    timestamp_utc: String,
    feature_id: String,
    contract_path: String,
    checks: Vec<FeatureVerifyCheckRow>,
    acceptance: Vec<FeatureVerifyAcceptanceRow>,
    summary: FeatureVerifySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureVerifySummary {
    checks_total: usize,
    checks_passed: usize,
    checks_failed: usize,
    required_checks_failed: usize,
    profile: String,
    acceptance_satisfied: bool,
    promotable: bool,
    promotion_reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct FeatureDispatchRunArtifacts {
    ledger: FeatureRunLedger,
    ledger_json: std::path::PathBuf,
    ledger_md: std::path::PathBuf,
    aborted_on_failure: bool,
}

#[derive(Debug, Clone)]
struct FeatureDispatchOptions {
    wait_secs: u64,
    outcome_grace_secs: Option<u64>,
    continue_on_failure: bool,
    rollback_on_failure: bool,
    cli_tool: Option<String>,
    cli_model: Option<String>,
    lane: Option<String>,
    risk: Option<String>,
    repo: Option<String>,
}

#[derive(Debug, Clone)]
struct FeatureRunnableTask {
    task: feature_contract::FeatureTask,
    lane: String,
    risk: String,
    repo: String,
    wait_secs: u64,
    outcome_grace_secs: u64,
    context: String,
}

#[derive(Debug, Clone)]
struct FeatureTaskDispatchOutcome {
    task_id: String,
    dispatch_task_id: String,
    status: FeatureRunStatus,
    result_summary: Option<String>,
}

#[derive(Debug, Clone)]
struct EvalGateStatus {
    suite: String,
    timestamp_utc: String,
    gate_ok: bool,
    report_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeaturePromotionDecision {
    timestamp_utc: String,
    feature_id: String,
    risk_tier: String,
    contract_path: String,
    dispatch_ledger_json: String,
    verify_ledger_json: String,
    real_gate_suite: Option<String>,
    real_gate_timestamp_utc: Option<String>,
    real_gate_ok: Option<bool>,
    real_gate_report_json: Option<String>,
    auto_promotable: bool,
    approval_required: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureGateLedger {
    timestamp_utc: String,
    feature_id: String,
    verify_profile: String,
    dispatch_ledger_json: String,
    verify_ledger_json: String,
    promote_decision_json: String,
    gate_ok: bool,
    reasons: Vec<String>,
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
        FeatureAction::Verify { file, profile } => {
            let profile = normalize_verify_profile(&profile)?;
            let contract = feature_contract::load_feature_contract(&file)?;
            let ledger = run_feature_verify(&contract, &file, &profile)?;
            let (json_path, md_path) = write_feature_verify_artifacts(&ledger)?;
            println!("feature_verify_json={}", json_path.display());
            println!("feature_verify_md={}", md_path.display());
            println!(
                "feature_id={} verify profile={} checks_total={} checks_failed={} required_checks_failed={} acceptance_satisfied={} promotable={}",
                contract.feature_id,
                profile,
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
        FeatureAction::Promote {
            file,
            dispatch_ledger,
            verify_ledger,
        } => {
            let contract = feature_contract::load_feature_contract(&file)?;
            let dispatch_path =
                resolve_dispatch_ledger_path(&contract.feature_id, dispatch_ledger.as_deref())?;
            let verify_path =
                resolve_verify_ledger_path(&contract.feature_id, verify_ledger.as_deref())?;
            let dispatch_ledger = read_dispatch_ledger(&dispatch_path)?;
            let verify_ledger = read_verify_ledger(&verify_path)?;
            if dispatch_ledger.feature_id != contract.feature_id {
                anyhow::bail!(
                    "Dispatch ledger feature_id mismatch: expected '{}' got '{}'",
                    contract.feature_id,
                    dispatch_ledger.feature_id
                );
            }
            if verify_ledger.feature_id != contract.feature_id {
                anyhow::bail!(
                    "Verify ledger feature_id mismatch: expected '{}' got '{}'",
                    contract.feature_id,
                    verify_ledger.feature_id
                );
            }
            let risk = contract
                .risk
                .clone()
                .unwrap_or_else(|| "medium".to_string());
            let real_gate = latest_real_gate_status()?;
            let decision = build_feature_promotion_decision(
                &contract,
                &risk,
                &file,
                &dispatch_path,
                &dispatch_ledger,
                &verify_path,
                &verify_ledger,
                real_gate.as_ref(),
            );
            let (json_path, md_path) = write_feature_promotion_artifacts(&decision)?;
            println!("feature_promote_json={}", json_path.display());
            println!("feature_promote_md={}", md_path.display());
            println!(
                "feature_id={} promote auto_promotable={} approval_required={} risk={}",
                decision.feature_id,
                decision.auto_promotable,
                decision.approval_required,
                decision.risk_tier
            );
            if !decision.reasons.is_empty() {
                println!("feature_promote_reasons={}", decision.reasons.join(" | "));
            }
        }
        FeatureAction::Dispatch {
            file,
            wait_secs,
            outcome_grace_secs,
            continue_on_failure,
            rollback_on_failure,
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
            let dispatch = run_feature_dispatch_flow(
                config.clone(),
                memory.clone(),
                &contract,
                &file,
                FeatureDispatchOptions {
                    wait_secs,
                    outcome_grace_secs,
                    continue_on_failure,
                    rollback_on_failure,
                    cli_tool,
                    cli_model,
                    lane,
                    risk,
                    repo,
                },
            )
            .await?;
            let ledger = &dispatch.ledger;
            println!("feature_ledger_json={}", dispatch.ledger_json.display());
            println!("feature_ledger_md={}", dispatch.ledger_md.display());
            println!(
                "feature_id={} summary succeeded={} failed={} skipped={} promotable={}",
                contract.feature_id,
                ledger.summary.succeeded,
                ledger.summary.failed,
                ledger.summary.skipped,
                ledger.summary.promotable
            );
            if !ledger.summary.promotion_reasons.is_empty() {
                println!(
                    "feature_promotion_reasons={}",
                    ledger.summary.promotion_reasons.join(" | ")
                );
            }
            if !ledger.rollback_commits.is_empty() {
                let rollback_failed = ledger
                    .rollback_commits
                    .iter()
                    .filter(|row| !row.reverted)
                    .count();
                println!(
                    "feature_rollback_commits={} feature_rollback_failed={}",
                    ledger.rollback_commits.len(),
                    rollback_failed
                );
            }
            if dispatch.aborted_on_failure {
                anyhow::bail!(
                    "Feature dispatch stopped early due to failed task (use --continue-on-failure to continue)."
                );
            }
            if ledger.summary.failed > 0 {
                anyhow::bail!(
                    "Feature dispatch completed with {} failed task(s).",
                    ledger.summary.failed
                );
            }
            if !ledger.summary.acceptance_satisfied {
                anyhow::bail!(
                    "Feature dispatch completed but acceptance criteria are not fully satisfied."
                );
            }
        }
        FeatureAction::Gate {
            file,
            wait_secs,
            outcome_grace_secs,
            continue_on_failure,
            cli_tool,
            cli_model,
            lane,
            risk,
            repo,
            verify_profile,
        } => {
            let verify_profile = normalize_verify_profile(&verify_profile)?;
            let contract = feature_contract::load_feature_contract(&file)?;
            let batches = contract.execution_batches()?;
            if batches.is_empty() {
                println!("feature_id={} nothing_to_run=true", contract.feature_id);
                return Ok(());
            }

            let dispatch = run_feature_dispatch_flow(
                config.clone(),
                memory.clone(),
                &contract,
                &file,
                FeatureDispatchOptions {
                    wait_secs,
                    outcome_grace_secs,
                    continue_on_failure,
                    rollback_on_failure: false,
                    cli_tool,
                    cli_model,
                    lane,
                    risk: risk.clone(),
                    repo,
                },
            )
            .await?;
            println!("feature_ledger_json={}", dispatch.ledger_json.display());
            println!("feature_ledger_md={}", dispatch.ledger_md.display());

            let verify = run_feature_verify(&contract, &file, &verify_profile)?;
            let (verify_json, verify_md) = write_feature_verify_artifacts(&verify)?;
            println!("feature_verify_json={}", verify_json.display());
            println!("feature_verify_md={}", verify_md.display());

            let risk_for_policy = risk
                .or_else(|| contract.risk.clone())
                .unwrap_or_else(|| "medium".to_string());
            validate_risk(&risk_for_policy)?;
            let real_gate = latest_real_gate_status()?;
            let decision = build_feature_promotion_decision(
                &contract,
                &risk_for_policy,
                &file,
                &dispatch.ledger_json,
                &dispatch.ledger,
                &verify_json,
                &verify,
                real_gate.as_ref(),
            );
            let (promote_json, promote_md) = write_feature_promotion_artifacts(&decision)?;
            println!("feature_promote_json={}", promote_json.display());
            println!("feature_promote_md={}", promote_md.display());

            let gate = build_feature_gate_ledger(
                &contract.feature_id,
                &verify_profile,
                &dispatch.ledger_json,
                &verify_json,
                &promote_json,
                &decision,
            );
            let (gate_json, gate_md) = write_feature_gate_artifacts(&gate)?;
            println!("feature_gate_json={}", gate_json.display());
            println!("feature_gate_md={}", gate_md.display());
            println!(
                "feature_id={} gate_ok={} auto_promotable={} approval_required={} verify_profile={}",
                contract.feature_id,
                gate.gate_ok,
                decision.auto_promotable,
                decision.approval_required,
                verify_profile
            );
            if !gate.reasons.is_empty() {
                println!("feature_gate_reasons={}", gate.reasons.join(" | "));
            }
            if !gate.gate_ok {
                anyhow::bail!("Feature gate failed.");
            }
        }
    }
    Ok(())
}

fn normalize_verify_profile(profile: &str) -> anyhow::Result<String> {
    match profile {
        "fast" | "strict" => Ok(profile.to_string()),
        _ => anyhow::bail!("Invalid verify profile '{}'. Use: fast | strict", profile),
    }
}

fn verify_check_in_profile(check: &feature_contract::VerificationCheck, profile: &str) -> bool {
    match profile {
        "fast" => check.profile == "fast",
        // strict profile is a superset: it runs both strict and fast checks.
        "strict" => matches!(check.profile.as_str(), "strict" | "fast"),
        _ => false,
    }
}

fn feature_batch_configured_parallelism_from_raw(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 4))
        .unwrap_or(2)
}

fn feature_batch_configured_parallelism() -> usize {
    feature_batch_configured_parallelism_from_raw(
        std::env::var("ATHENA_FEATURE_BATCH_CONCURRENCY").ok().as_deref(),
    )
}

fn feature_batch_dynamic_parallelism(
    configured: usize,
    metrics: Option<&crate::introspect::SystemMetrics>,
) -> (usize, String) {
    let configured = configured.clamp(1, 4);
    let Some(metrics) = metrics else {
        return (configured, "metrics_unavailable".to_string());
    };

    if metrics.active_containers > 4 {
        return (
            1,
            format!("active_containers={} > 4", metrics.active_containers),
        );
    }

    if metrics.total_memory_bytes == 0 || metrics.rss_bytes == 0 {
        return (configured, "rss_unavailable".to_string());
    }

    let rss_pct = (metrics.rss_bytes as f64 / metrics.total_memory_bytes as f64) * 100.0;
    if rss_pct > 80.0 {
        return (1, format!("rss_pct={:.1} > 80", rss_pct));
    }
    if rss_pct > 60.0 {
        return (
            configured.min(2),
            format!("rss_pct={:.1} > 60 -> min(2, configured)", rss_pct),
        );
    }
    (configured, format!("rss_pct={:.1} <= 60", rss_pct))
}

fn latest_eval_gate_status(
    history_path: &std::path::Path,
    suite: &str,
) -> anyhow::Result<Option<EvalGateStatus>> {
    let raw = match std::fs::read_to_string(history_path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            anyhow::bail!(
                "Failed to read eval history '{}': {}",
                history_path.display(),
                e
            )
        }
    };

    let mut latest: Option<EvalGateStatus> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value
            .get("suite")
            .and_then(|v| v.as_str())
            .map(|s| s == suite)
            != Some(true)
        {
            continue;
        }
        let timestamp_utc = value
            .get("timestamp_utc")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if timestamp_utc.is_empty() {
            continue;
        }
        let candidate = EvalGateStatus {
            suite: suite.to_string(),
            timestamp_utc,
            gate_ok: value
                .get("gate_ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            report_json: value
                .get("report_json")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };
        let replace = latest
            .as_ref()
            .map(|current| candidate.timestamp_utc > current.timestamp_utc)
            .unwrap_or(true);
        if replace {
            latest = Some(candidate);
        }
    }
    Ok(latest)
}

fn latest_real_gate_status() -> anyhow::Result<Option<EvalGateStatus>> {
    latest_eval_gate_status(
        std::path::Path::new("eval/results/history.jsonl"),
        "athena-core-v2-real",
    )
}

async fn run_feature_task_dispatch(
    handle: core::CoreHandle,
    config: Config,
    runnable: FeatureRunnableTask,
) -> anyhow::Result<FeatureTaskDispatchOutcome> {
    let mut pulse_rx = handle.pulse_bus.subscribe();
    let task_dispatch_id = handle
        .dispatch_task(core::AutonomousTask {
            goal: runnable.task.goal.clone(),
            context: runnable.context,
            ghost: runnable.task.ghost.clone(),
            target: crate::pulse::PulseTarget::Broadcast,
            lane: runnable.lane,
            risk_tier: runnable.risk,
            repo: runnable.repo,
            task_id: None,
        })
        .await?;

    println!(
        "task={} dispatched_task_id={} wait_secs={} outcome_grace_secs={}",
        runnable.task.id, task_dispatch_id, runnable.wait_secs, runnable.outcome_grace_secs
    );
    let wait =
        wait_for_autonomous_pulse(&mut pulse_rx, &task_dispatch_id, runnable.wait_secs).await;
    let result_summary = match &wait {
        WaitForAutonomousOutcome::Received(content) => clip_feature_result_summary(content),
        WaitForAutonomousOutcome::TimedOut | WaitForAutonomousOutcome::ChannelClosed => None,
    };
    let status = resolve_feature_run_status_after_wait(
        &config,
        &task_dispatch_id,
        wait,
        runnable.outcome_grace_secs,
    )
    .await?;

    Ok(FeatureTaskDispatchOutcome {
        task_id: runnable.task.id,
        dispatch_task_id: task_dispatch_id,
        status,
        result_summary,
    })
}

async fn run_feature_dispatch_flow(
    config: Config,
    memory: Arc<MemoryStore>,
    contract: &feature_contract::FeatureContract,
    contract_path: &std::path::Path,
    opts: FeatureDispatchOptions,
) -> anyhow::Result<FeatureDispatchRunArtifacts> {
    if let Some(l) = opts.lane.as_deref() {
        validate_lane(l)?;
    }
    if let Some(r) = opts.risk.as_deref() {
        validate_risk(r)?;
    }

    let handle = AthenaCore::start(config.clone(), memory).await?;
    if opts.cli_tool.is_some() || opts.cli_model.is_some() {
        let mut knobs = handle
            .knobs
            .write()
            .map_err(|_| anyhow::anyhow!("Failed to lock runtime knobs"))?;
        if let Some(tool) = opts.cli_tool.as_deref() {
            knobs.set("cli_tool", tool).map_err(anyhow::Error::msg)?;
            eprintln!("Feature override: cli_tool={}", tool);
        }
        if let Some(model) = opts.cli_model.as_deref() {
            knobs.set("cli_model", model).map_err(anyhow::Error::msg)?;
            eprintln!("Feature override: cli_model={}", model);
        }
    }

    let batches = contract.execution_batches()?;
    println!(
        "feature_id={} mode=dispatch batches={} continue_on_failure={} rollback_on_failure={}",
        contract.feature_id,
        batches.len(),
        opts.continue_on_failure,
        opts.rollback_on_failure
    );
    let configured_parallelism = feature_batch_configured_parallelism();
    let metrics_snapshot = handle.metrics.read().ok().map(|m| m.clone());
    let (dispatch_parallelism, parallelism_reason) =
        feature_batch_dynamic_parallelism(configured_parallelism, metrics_snapshot.as_ref());
    let metrics_for_log = metrics_snapshot.unwrap_or_default();
    let rss_pct = if metrics_for_log.total_memory_bytes > 0 && metrics_for_log.rss_bytes > 0 {
        Some((metrics_for_log.rss_bytes as f64 / metrics_for_log.total_memory_bytes as f64) * 100.0)
    } else {
        None
    };
    println!(
        "feature_id={} batch concurrency={} (dynamic) configured={} reason={} rss_bytes={} total_memory_bytes={} rss_pct={} active_containers={}",
        contract.feature_id,
        dispatch_parallelism,
        configured_parallelism,
        parallelism_reason,
        metrics_for_log.rss_bytes,
        metrics_for_log.total_memory_bytes,
        rss_pct
            .map(|v| format!("{:.1}", v))
            .unwrap_or_else(|| "-".to_string()),
        metrics_for_log.active_containers
    );

    let mut statuses: std::collections::HashMap<String, FeatureRunStatus> =
        std::collections::HashMap::new();
    let rollback_commit_tag = opts
        .rollback_on_failure
        .then(|| format!("athena-feature-run:{}", uuid::Uuid::new_v4()));
    let rollback_repo_root = if opts.rollback_on_failure {
        let repo_root = resolve_repo_root().await?;
        ensure_clean_working_tree(&repo_root).await?;
        Some(repo_root)
    } else {
        None
    };
    let mut rollback_last_head = if let Some(repo_root) = rollback_repo_root.as_ref() {
        current_git_head(repo_root).await?
    } else {
        None
    };
    let mut rollback_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rollback_tracked_commits: Vec<String> = Vec::new();
    let mut predecessor_summaries: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut dispatch_ids: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let default_lane = opts
        .lane
        .clone()
        .or_else(|| contract.lane.clone())
        .unwrap_or_else(|| "delivery".to_string());
    let default_risk = opts
        .risk
        .clone()
        .or_else(|| contract.risk.clone())
        .unwrap_or_else(|| "medium".to_string());
    let default_repo = opts
        .repo
        .clone()
        .or_else(|| contract.repo.clone())
        .unwrap_or_else(kpi::default_repo_name);
    validate_lane(&default_lane)?;
    validate_risk(&default_risk)?;
    let mut aborted_on_failure = false;

    for (idx, batch) in batches.iter().enumerate() {
        println!("batch={} tasks={}", idx + 1, batch.join(","));
        let mut runnable = Vec::new();
        for task_id in batch {
            let task = contract
                .task_by_id(task_id)
                .ok_or_else(|| anyhow::anyhow!("Task '{}' missing from contract", task_id))?;
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
            let task_wait = task.wait_secs.unwrap_or(opts.wait_secs);
            let task_grace = compute_feature_outcome_grace_secs(
                task,
                &lane,
                &risk,
                task_wait,
                opts.outcome_grace_secs,
            );
            let mut context = build_feature_task_context(contract, task, &predecessor_summaries);
            if let Some(tag) = rollback_commit_tag.as_deref() {
                context = append_feature_rollback_commit_policy_context(context, tag);
            }
            runnable.push(FeatureRunnableTask {
                task: task.clone(),
                lane,
                risk,
                repo,
                wait_secs: task_wait,
                outcome_grace_secs: task_grace,
                context,
            });
        }
        if runnable.is_empty() {
            continue;
        }

        let max_parallel = std::cmp::min(dispatch_parallelism, runnable.len()).max(1);
        println!(
            "batch={} runnable={} max_parallel={}",
            idx + 1,
            runnable.len(),
            max_parallel
        );

        let mut queue = runnable.into_iter();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..max_parallel {
            if let Some(next) = queue.next() {
                set.spawn(run_feature_task_dispatch(
                    handle.clone(),
                    config.clone(),
                    next,
                ));
            }
        }

        let mut stop_spawning = false;
        while let Some(joined) = set.join_next().await {
            let outcome =
                joined.map_err(|e| anyhow::anyhow!("feature task worker join failed: {}", e))??;
            match &outcome.status {
                FeatureRunStatus::Succeeded => {
                    println!("task={} result=succeeded", outcome.task_id)
                }
                FeatureRunStatus::Failed(reason) => println!(
                    "task={} result=failed reason={}",
                    outcome.task_id,
                    reason.replace('\n', " ")
                ),
                FeatureRunStatus::Skipped(reason) => println!(
                    "task={} result=skipped reason={}",
                    outcome.task_id,
                    reason.replace('\n', " ")
                ),
            }
            let failed = matches!(outcome.status, FeatureRunStatus::Failed(_));
            if matches!(outcome.status, FeatureRunStatus::Succeeded) {
                if let Some(summary) = outcome.result_summary.clone() {
                    predecessor_summaries.insert(outcome.task_id.clone(), summary);
                }
                if let Some(repo_root) = rollback_repo_root.as_ref() {
                    let new_commits = track_feature_commits_since(
                        repo_root,
                        &mut rollback_last_head,
                        &mut rollback_seen,
                        rollback_commit_tag.as_deref(),
                    )
                    .await?;
                    if !new_commits.is_empty() {
                        println!(
                            "task={} tracked_new_commits={}",
                            outcome.task_id,
                            new_commits.join(",")
                        );
                        rollback_tracked_commits.extend(new_commits);
                    }
                }
            }
            dispatch_ids.insert(outcome.task_id.clone(), outcome.dispatch_task_id);
            statuses.insert(outcome.task_id.clone(), outcome.status);
            if failed && !opts.continue_on_failure {
                aborted_on_failure = true;
                stop_spawning = true;
            }
            if !stop_spawning {
                if let Some(next) = queue.next() {
                    set.spawn(run_feature_task_dispatch(
                        handle.clone(),
                        config.clone(),
                        next,
                    ));
                }
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

    let rollback_commits = if failed > 0 && opts.rollback_on_failure {
        if let Some(repo_root) = rollback_repo_root.as_ref() {
            println!(
                "feature_id={} rollback_on_failure=true failed_tasks={} tracked_commits={}",
                contract.feature_id,
                failed,
                rollback_tracked_commits.len()
            );
            rollback_feature_commits(repo_root, &rollback_tracked_commits).await
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let mut ledger = build_feature_run_ledger(
        contract,
        contract_path,
        &statuses,
        &dispatch_ids,
        succeeded,
        failed,
        skipped,
    );
    ledger.rollback_commits = rollback_commits;
    let (ledger_json, ledger_md) = write_feature_ledger_artifacts(&ledger)?;
    Ok(FeatureDispatchRunArtifacts {
        ledger,
        ledger_json,
        ledger_md,
        aborted_on_failure,
    })
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

fn compute_feature_outcome_grace_secs(
    task: &feature_contract::FeatureTask,
    lane: &str,
    risk_tier: &str,
    wait_secs: u64,
    override_grace_secs: Option<u64>,
) -> u64 {
    if let Some(v) = override_grace_secs {
        return v.max(1);
    }

    let mut base = match risk_tier {
        "low" => 180,
        "medium" => 300,
        "high" => 480,
        _ => 240,
    };
    if lane == "self_improvement" {
        base += 60;
    }

    let ghost = task.ghost.as_deref().unwrap_or("auto");
    if matches!(ghost, "coder" | "architect") {
        base += 180;
    } else if ghost == "scout" {
        base += 60;
    }

    let goal = task.goal.to_ascii_lowercase();
    if goal.contains("test")
        || goal.contains("refactor")
        || goal.contains("compile")
        || goal.contains("cargo")
        || goal.contains("benchmark")
        || goal.contains("integration")
        || goal.contains("migrat")
        || goal.contains("implement")
    {
        base += 120;
    }

    let wait_scaled = wait_secs.saturating_mul(2);
    base.max(wait_scaled).clamp(60, 1800)
}

fn clip_feature_result_summary(summary: &str) -> Option<String> {
    let normalized = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(normalized.chars().take(500).collect())
}

fn build_feature_task_context(
    contract: &feature_contract::FeatureContract,
    task: &feature_contract::FeatureTask,
    predecessor_summaries: &std::collections::HashMap<String, String>,
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
    let mut predecessor_lines = Vec::new();
    for dep in &task.depends_on {
        if let Some(summary) = predecessor_summaries
            .get(dep)
            .and_then(|summary| clip_feature_result_summary(summary))
        {
            predecessor_lines.push(format!("- {}: {}", dep, summary));
        }
    }
    if !predecessor_lines.is_empty() {
        context.push_str("\nPrevious task results:");
        for line in predecessor_lines {
            context.push('\n');
            context.push_str(&line);
        }
    }
    context
}

fn append_feature_rollback_commit_policy_context(mut context: String, commit_tag: &str) -> String {
    context.push_str(&format!("\n[feature_rollback_commit_tag:{}]", commit_tag));
    context.push_str(
        "\nIf you create git commits for this feature task, include the rollback tag in the commit message.",
    );
    context
}

async fn ensure_clean_working_tree(repo_root: &std::path::Path) -> anyhow::Result<()> {
    let status = run_command_capture(repo_root, "git", &args(&["status", "--porcelain"]), 30).await;
    if !command_succeeded(&status) {
        anyhow::bail!(
            "rollback-on-failure precheck failed: could not read git status ({})",
            tail_text(&command_combined_output(&status), 300)
        );
    }
    let dirty = status
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string());
    if let Some(first) = dirty {
        anyhow::bail!(
            "rollback-on-failure requires a clean working tree; found local changes (first entry: '{}')",
            first
        );
    }
    Ok(())
}

async fn current_git_head(repo_root: &std::path::Path) -> anyhow::Result<Option<String>> {
    let rev = run_command_capture(repo_root, "git", &args(&["rev-parse", "HEAD"]), 30).await;
    if !command_succeeded(&rev) {
        return Ok(None);
    }
    let sha = rev.stdout.trim();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha.to_string()))
    }
}

async fn track_feature_commits_since(
    repo_root: &std::path::Path,
    last_head: &mut Option<String>,
    seen: &mut std::collections::HashSet<String>,
    commit_tag: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let Some(current_head) = current_git_head(repo_root).await? else {
        return Ok(Vec::new());
    };
    let Some(previous_head) = last_head.clone() else {
        *last_head = Some(current_head);
        return Ok(Vec::new());
    };
    if previous_head == current_head {
        return Ok(Vec::new());
    }
    let list = run_command_capture(
        repo_root,
        "git",
        &args(&[
            "rev-list",
            "--reverse",
            &format!("{}..{}", previous_head, current_head),
        ]),
        30,
    )
    .await;
    *last_head = Some(current_head);
    if !command_succeeded(&list) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in list.stdout.lines() {
        let sha = line.trim();
        if sha.is_empty() {
            continue;
        }
        if let Some(tag) = commit_tag {
            if !commit_message_contains_tag(repo_root, sha, tag).await? {
                continue;
            }
        }
        if seen.insert(sha.to_string()) {
            out.push(sha.to_string());
        }
    }
    Ok(out)
}

async fn commit_message_contains_tag(
    repo_root: &std::path::Path,
    sha: &str,
    tag: &str,
) -> anyhow::Result<bool> {
    let show = run_command_capture(
        repo_root,
        "git",
        &args(&["show", "-s", "--format=%B", sha]),
        30,
    )
    .await;
    if !command_succeeded(&show) {
        return Ok(false);
    }
    Ok(show.stdout.contains(tag))
}

async fn abort_git_revert_if_needed(repo_root: &std::path::Path) -> Option<String> {
    let abort = run_command_capture(repo_root, "git", &args(&["revert", "--abort"]), 30).await;
    if command_succeeded(&abort) {
        return None;
    }
    let output = command_combined_output(&abort);
    let lowered = output.to_ascii_lowercase();
    if lowered.contains("no cherry-pick or revert in progress")
        || lowered.contains("there is no merge to abort")
    {
        return None;
    }
    Some(tail_text(&output, 300))
}

async fn rollback_feature_commits(
    repo_root: &std::path::Path,
    commit_shas: &[String],
) -> Vec<FeatureRollbackLedgerRow> {
    let mut rows = Vec::new();
    for sha in commit_shas.iter().rev() {
        let revert =
            run_command_capture(repo_root, "git", &args(&["revert", "--no-edit", sha]), 180).await;
        if command_succeeded(&revert) {
            println!("feature_rollback commit={} status=reverted", sha);
            rows.push(FeatureRollbackLedgerRow {
                commit_sha: sha.clone(),
                reverted: true,
                error: None,
            });
        } else {
            let mut error = tail_text(&command_combined_output(&revert), 400);
            if let Some(abort_error) = abort_git_revert_if_needed(repo_root).await {
                error.push_str(&format!(" | revert_abort_failed: {}", abort_error));
            }
            println!(
                "feature_rollback commit={} status=failed reason={}",
                sha,
                error.replace('\n', " ")
            );
            rows.push(FeatureRollbackLedgerRow {
                commit_sha: sha.clone(),
                reverted: false,
                error: Some(error),
            });
        }
    }
    rows
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
    let promotion_reasons =
        feature_dispatch_promotion_reasons(failed, skipped, all_covered, all_satisfied);

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
        rollback_commits: Vec::new(),
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

fn feature_dispatch_promotion_reasons(
    failed: usize,
    skipped: usize,
    all_covered: bool,
    all_satisfied: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if failed > 0 {
        reasons.push(format!("{} task(s) failed", failed));
    }
    if skipped > 0 {
        reasons.push(format!("{} task(s) skipped", skipped));
    }
    if !all_covered {
        reasons.push("some acceptance criteria have no task coverage".to_string());
    }
    if !all_satisfied {
        reasons.push("some acceptance criteria have no succeeded mapped task".to_string());
    }
    reasons
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
    if !ledger.rollback_commits.is_empty() {
        let rollback_failed = ledger
            .rollback_commits
            .iter()
            .filter(|row| !row.reverted)
            .count();
        out.push_str(&format!(
            "- rollback_commits: total={} failed={}\n",
            ledger.rollback_commits.len(),
            rollback_failed
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
    if !ledger.rollback_commits.is_empty() {
        out.push_str("\n## Rollback Commits\n\n");
        out.push_str("| commit_sha | reverted | error |\n");
        out.push_str("|---|---|---|\n");
        for row in &ledger.rollback_commits {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                row.commit_sha,
                row.reverted,
                row.error.clone().unwrap_or_else(|| "-".to_string())
            ));
        }
    }
    out
}

fn run_feature_verify(
    contract: &feature_contract::FeatureContract,
    contract_path: &std::path::Path,
    profile: &str,
) -> anyhow::Result<FeatureVerifyLedger> {
    if contract.verification_checks.is_empty() {
        anyhow::bail!(
            "feature '{}' has no verification_checks; add checks mapped to acceptance criteria",
            contract.feature_id
        );
    }
    let profile = normalize_verify_profile(profile)?;

    let mut check_rows = Vec::new();
    let mut checks_passed = 0usize;
    let mut checks_failed = 0usize;
    let mut required_checks_failed = 0usize;
    let selected_checks = contract
        .verification_checks
        .iter()
        .filter(|check| verify_check_in_profile(check, &profile))
        .collect::<Vec<_>>();
    if selected_checks.is_empty() {
        anyhow::bail!(
            "feature '{}' has no verification checks for profile '{}'",
            contract.feature_id,
            profile
        );
    }

    for check in selected_checks {
        let output = std::process::Command::new("sh")
            .arg("-c")
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
            profile: check.profile.clone(),
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
            profile,
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
    out.push_str(&format!("- profile: `{}`\n", ledger.summary.profile));
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
    out.push_str(
        "| check_id | profile | status | required | exit_code | mapped_acceptance | command |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|\n");
    for row in &ledger.checks {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | `{}` |\n",
            row.check_id,
            row.profile,
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

fn build_feature_promotion_decision(
    contract: &feature_contract::FeatureContract,
    risk_tier: &str,
    contract_path: &std::path::Path,
    dispatch_path: &std::path::Path,
    dispatch_ledger: &FeatureRunLedger,
    verify_path: &std::path::Path,
    verify_ledger: &FeatureVerifyLedger,
    real_gate: Option<&EvalGateStatus>,
) -> FeaturePromotionDecision {
    let mut reasons = Vec::new();
    if !dispatch_ledger.summary.promotable {
        reasons.push(format!(
            "dispatch ledger not promotable: {}",
            if dispatch_ledger.summary.promotion_reasons.is_empty() {
                "no details".to_string()
            } else {
                dispatch_ledger.summary.promotion_reasons.join(" | ")
            }
        ));
    }
    if !verify_ledger.summary.promotable {
        reasons.push(format!(
            "verify ledger not promotable: {}",
            if verify_ledger.summary.promotion_reasons.is_empty() {
                "no details".to_string()
            } else {
                verify_ledger.summary.promotion_reasons.join(" | ")
            }
        ));
    }

    let mut auto_promotable =
        dispatch_ledger.summary.promotable && verify_ledger.summary.promotable;
    let (real_gate_suite, real_gate_timestamp_utc, real_gate_ok, real_gate_report_json) =
        if let Some(gate) = real_gate {
            if !gate.gate_ok {
                reasons.push(format!(
                    "latest real eval gate '{}' at {} is FAIL",
                    gate.suite, gate.timestamp_utc
                ));
                auto_promotable = false;
            }
            (
                Some(gate.suite.clone()),
                Some(gate.timestamp_utc.clone()),
                Some(gate.gate_ok),
                gate.report_json.clone(),
            )
        } else {
            reasons.push(
                "no real eval gate result found (expected suite 'athena-core-v2-real')".to_string(),
            );
            auto_promotable = false;
            (None, None, None, None)
        };
    let approval_required = !matches!(risk_tier, "low");
    if approval_required {
        reasons.push(format!(
            "risk tier '{}' requires human approval (PR-only)",
            risk_tier
        ));
        auto_promotable = false;
    }

    FeaturePromotionDecision {
        timestamp_utc: chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        feature_id: contract.feature_id.clone(),
        risk_tier: risk_tier.to_string(),
        contract_path: contract_path.display().to_string(),
        dispatch_ledger_json: dispatch_path.display().to_string(),
        verify_ledger_json: verify_path.display().to_string(),
        real_gate_suite,
        real_gate_timestamp_utc,
        real_gate_ok,
        real_gate_report_json,
        auto_promotable,
        approval_required,
        reasons,
    }
}

fn write_feature_promotion_artifacts(
    decision: &FeaturePromotionDecision,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let out_dir = std::path::PathBuf::from("eval/results");
    std::fs::create_dir_all(&out_dir)?;
    let safe_feature_id = decision
        .feature_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let base = format!(
        "feature-promote-{}-{}",
        safe_feature_id, decision.timestamp_utc
    );
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    std::fs::write(&json_path, serde_json::to_string_pretty(decision)?)?;
    std::fs::write(&md_path, render_feature_promotion_markdown(decision))?;
    Ok((json_path, md_path))
}

fn render_feature_promotion_markdown(decision: &FeaturePromotionDecision) -> String {
    let mut out = String::new();
    out.push_str("# Feature Promotion Decision\n\n");
    out.push_str(&format!("- feature_id: `{}`\n", decision.feature_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", decision.timestamp_utc));
    out.push_str(&format!("- risk_tier: `{}`\n", decision.risk_tier));
    out.push_str(&format!(
        "- auto_promotable: `{}`\n",
        decision.auto_promotable
    ));
    out.push_str(&format!(
        "- approval_required: `{}`\n",
        decision.approval_required
    ));
    out.push_str(&format!(
        "- dispatch_ledger_json: `{}`\n",
        decision.dispatch_ledger_json
    ));
    out.push_str(&format!(
        "- verify_ledger_json: `{}`\n",
        decision.verify_ledger_json
    ));
    if let Some(suite) = &decision.real_gate_suite {
        out.push_str(&format!("- real_gate_suite: `{}`\n", suite));
    }
    if let Some(ts) = &decision.real_gate_timestamp_utc {
        out.push_str(&format!("- real_gate_timestamp_utc: `{}`\n", ts));
    }
    if let Some(ok) = decision.real_gate_ok {
        out.push_str(&format!("- real_gate_ok: `{}`\n", ok));
    }
    if let Some(report) = &decision.real_gate_report_json {
        out.push_str(&format!("- real_gate_report_json: `{}`\n", report));
    }
    if !decision.reasons.is_empty() {
        out.push_str("- reasons:\n");
        for reason in &decision.reasons {
            out.push_str(&format!("  - {}\n", reason));
        }
    }
    out
}

fn build_feature_gate_ledger(
    feature_id: &str,
    verify_profile: &str,
    dispatch_ledger_json: &std::path::Path,
    verify_ledger_json: &std::path::Path,
    promote_decision_json: &std::path::Path,
    decision: &FeaturePromotionDecision,
) -> FeatureGateLedger {
    let gate_ok = decision.auto_promotable && !decision.approval_required;
    let mut reasons = Vec::new();
    if !decision.auto_promotable {
        reasons.push("promotion decision is not auto-promotable".to_string());
    }
    if decision.approval_required {
        reasons.push(format!(
            "risk tier '{}' requires human approval",
            decision.risk_tier
        ));
    }
    reasons.extend(decision.reasons.clone());
    reasons.sort();
    reasons.dedup();

    FeatureGateLedger {
        timestamp_utc: chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        feature_id: feature_id.to_string(),
        verify_profile: verify_profile.to_string(),
        dispatch_ledger_json: dispatch_ledger_json.display().to_string(),
        verify_ledger_json: verify_ledger_json.display().to_string(),
        promote_decision_json: promote_decision_json.display().to_string(),
        gate_ok,
        reasons,
    }
}

fn write_feature_gate_artifacts(
    ledger: &FeatureGateLedger,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let out_dir = std::path::PathBuf::from("eval/results");
    std::fs::create_dir_all(&out_dir)?;
    let safe_feature_id = ledger
        .feature_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let base = format!("feature-gate-{}-{}", safe_feature_id, ledger.timestamp_utc);
    let json_path = out_dir.join(format!("{}.json", base));
    let md_path = out_dir.join(format!("{}.md", base));
    std::fs::write(&json_path, serde_json::to_string_pretty(ledger)?)?;
    std::fs::write(&md_path, render_feature_gate_markdown(ledger))?;
    Ok((json_path, md_path))
}

fn render_feature_gate_markdown(ledger: &FeatureGateLedger) -> String {
    let mut out = String::new();
    out.push_str("# Feature Gate Ledger\n\n");
    out.push_str(&format!("- feature_id: `{}`\n", ledger.feature_id));
    out.push_str(&format!("- timestamp_utc: `{}`\n", ledger.timestamp_utc));
    out.push_str(&format!("- verify_profile: `{}`\n", ledger.verify_profile));
    out.push_str(&format!("- gate_ok: `{}`\n", ledger.gate_ok));
    out.push_str(&format!(
        "- dispatch_ledger_json: `{}`\n",
        ledger.dispatch_ledger_json
    ));
    out.push_str(&format!(
        "- verify_ledger_json: `{}`\n",
        ledger.verify_ledger_json
    ));
    out.push_str(&format!(
        "- promote_decision_json: `{}`\n",
        ledger.promote_decision_json
    ));
    if !ledger.reasons.is_empty() {
        out.push_str("- reasons:\n");
        for reason in &ledger.reasons {
            out.push_str(&format!("  - {}\n", reason));
        }
    }
    out
}

fn read_dispatch_ledger(path: &std::path::Path) -> anyhow::Result<FeatureRunLedger> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!("Failed to read dispatch ledger '{}': {}", path.display(), e)
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse dispatch ledger JSON '{}': {}",
            path.display(),
            e
        )
    })
}

fn read_verify_ledger(path: &std::path::Path) -> anyhow::Result<FeatureVerifyLedger> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read verify ledger '{}': {}", path.display(), e))?;
    serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse verify ledger JSON '{}': {}",
            path.display(),
            e
        )
    })
}

fn resolve_dispatch_ledger_path(
    feature_id: &str,
    override_path: Option<&std::path::Path>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let safe = sanitize_feature_id(feature_id);
    let prefix = format!("feature-{}-", safe);
    find_latest_result_json(&prefix, false)
}

fn resolve_verify_ledger_path(
    feature_id: &str,
    override_path: Option<&std::path::Path>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let safe = sanitize_feature_id(feature_id);
    let prefix = format!("feature-verify-{}-", safe);
    find_latest_result_json(&prefix, true)
}

fn find_latest_result_json(
    prefix: &str,
    allow_verify_prefix: bool,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = std::path::Path::new("eval/results");
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Failed to read '{}': {}", dir.display(), e))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        if !allow_verify_prefix && name.starts_with("feature-verify-") {
            continue;
        }
        candidates.push(path);
    }
    candidates.sort();
    candidates.pop().ok_or_else(|| {
        anyhow::anyhow!(
            "No ledger JSON found in eval/results for prefix '{}'. Run dispatch/verify first.",
            prefix
        )
    })
}

fn sanitize_feature_id(feature_id: &str) -> String {
    feature_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
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

fn parse_job_schedule(
    every: Option<u64>,
    cron: Option<String>,
    at: Option<String>,
) -> anyhow::Result<Schedule> {
    if let Some(secs) = every {
        return Ok(Schedule::Interval {
            every_secs: secs,
            jitter: 0.1,
        });
    }
    if let Some(expr) = cron {
        return Ok(Schedule::Cron { expression: expr });
    }
    if let Some(at_str) = at {
        let at_time = if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&at_str) {
            ts.with_timezone(&chrono::Utc)
        } else {
            let dur = humantime::parse_duration(&at_str)
                .map_err(|e| anyhow::anyhow!("Invalid --at '{}': {}", at_str, e))?;
            chrono::Utc::now()
                + chrono::Duration::from_std(dur)
                    .map_err(|e| anyhow::anyhow!("Invalid --at duration '{}': {}", at_str, e))?
        };
        return Ok(Schedule::OneShot { at: at_time });
    }
    anyhow::bail!("Specify exactly one of --every, --cron, or --at")
}

fn resolve_job_by_prefix(
    engine: &scheduler::CronEngine,
    prefix: &str,
) -> anyhow::Result<Option<String>> {
    let jobs = engine.list_jobs()?;
    Ok(jobs
        .iter()
        .find(|j| j.id.starts_with(prefix))
        .map(|j| j.id.clone()))
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
                    let ghost_info = j
                        .ghost
                        .as_deref()
                        .map(|g| format!(" — ghost: {}", g))
                        .unwrap_or_default();
                    let target_info = if j.target != "broadcast" {
                        format!(" — target: {}", j.target)
                    } else {
                        String::new()
                    };
                    println!(
                        "  [{}] {} ({}) — next: {} — {}{}{}",
                        &j.id[..8],
                        j.name,
                        status,
                        next,
                        j.prompt,
                        ghost_info,
                        target_info
                    );
                }
            }
        }
        JobsAction::Add {
            name,
            every,
            cron,
            at,
            prompt,
            ghost,
            target,
        } => {
            let schedule_count =
                u8::from(every.is_some()) + u8::from(cron.is_some()) + u8::from(at.is_some());
            if schedule_count != 1 {
                eprintln!("Specify exactly one of --every, --cron, or --at");
                return Ok(());
            }
            let schedule = parse_job_schedule(every, cron, at)?;
            let id = engine.create_job(&name, schedule, &prompt, ghost.as_deref(), &target)?;
            println!("Created job: {} ({})", name, &id[..8]);
        }
        JobsAction::Delete { id } => {
            if let Some(full_id) = resolve_job_by_prefix(engine, &id)? {
                engine.delete_job(&full_id)?;
                println!("Deleted job: {}", &full_id[..8]);
            } else {
                println!("Job not found: {}", id);
            }
        }
        JobsAction::Enable { id } => {
            if let Some(full_id) = resolve_job_by_prefix(engine, &id)? {
                engine.toggle_job(&full_id, true)?;
                println!("Enabled job: {}", &full_id[..8]);
            } else {
                println!("Job not found: {}", id);
            }
        }
        JobsAction::Disable { id } => {
            if let Some(full_id) = resolve_job_by_prefix(engine, &id)? {
                engine.toggle_job(&full_id, false)?;
                println!("Disabled job: {}", &full_id[..8]);
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
        WaitForAutonomousOutcome::Received(_) => Ok(()),
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
            return WaitForAutonomousOutcome::Received(pulse.content);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitForAutonomousOutcome {
    Received(String),
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

fn feature_status_from_terminal(status: &str) -> FeatureRunStatus {
    if status == "succeeded" {
        FeatureRunStatus::Succeeded
    } else {
        FeatureRunStatus::Failed(format!("terminal_status={}", status))
    }
}

async fn resolve_feature_run_status_after_wait(
    config: &Config,
    task_id: &str,
    wait_outcome: WaitForAutonomousOutcome,
    outcome_grace_secs: u64,
) -> anyhow::Result<FeatureRunStatus> {
    match wait_outcome {
        WaitForAutonomousOutcome::Received(_) => {
            match wait_for_terminal_outcome_status(config, task_id, 10).await? {
                Some(status) => Ok(feature_status_from_terminal(&status)),
                None => {
                    mark_dispatch_task_failed_if_started(
                        config,
                        task_id,
                        OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT,
                    );
                    Ok(FeatureRunStatus::Failed(
                        OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT.to_string(),
                    ))
                }
            }
        }
        WaitForAutonomousOutcome::TimedOut => {
            // Grace period: even if pulse timed out, the task may still finalize in DB.
            if let Some(status) =
                wait_for_terminal_outcome_status(config, task_id, outcome_grace_secs).await?
            {
                return Ok(feature_status_from_terminal(&status));
            }
            mark_dispatch_task_failed_if_started(
                config,
                task_id,
                OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT,
            );
            Ok(FeatureRunStatus::Failed(
                OUTCOME_REASON_OUTCOME_WAIT_TIMEOUT.to_string(),
            ))
        }
        WaitForAutonomousOutcome::ChannelClosed => {
            if let Some(status) =
                wait_for_terminal_outcome_status(config, task_id, outcome_grace_secs).await?
            {
                return Ok(feature_status_from_terminal(&status));
            }
            mark_dispatch_task_failed_if_started(
                config,
                task_id,
                OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED,
            );
            Ok(FeatureRunStatus::Failed(
                OUTCOME_REASON_DISPATCH_CHANNEL_CLOSED.to_string(),
            ))
        }
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
        append_feature_rollback_commit_policy_context, build_feature_promotion_decision,
        build_feature_run_ledger, build_feature_task_context, commit_message_contains_tag,
        build_self_build_review_checklist, classify_chat_command,
        compute_feature_outcome_grace_secs, ensure_clean_working_tree,
        evaluate_self_build_guardrails, feature_batch_configured_parallelism_from_raw,
        feature_batch_dynamic_parallelism, resolve_self_build_ci_monitor,
        latest_eval_gate_status, parse_dispatch_task_id, parse_git_status_paths,
        plan_self_build_promotion, pulse_matches_task_id, run_feature_verify,
        render_feature_ledger_markdown, rollback_feature_commits, track_feature_commits_since,
        wait_for_autonomous_pulse, ChatCommand, FeatureRunStatus, SelfBuildPromoteMode,
        WaitForAutonomousOutcome,
    };
    use crate::feature_contract::{
        AcceptanceCriterion, FeatureContract, FeatureTask, VerificationCheck,
    };
    use crate::introspect::SystemMetrics;
    use crate::pulse::{Pulse, PulseSource, Urgency};
    use std::collections::{HashMap, HashSet};
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
        assert_eq!(res, WaitForAutonomousOutcome::Received("match".to_string()));
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
                    profile: "fast".to_string(),
                    mapped_acceptance: vec!["AC-1".to_string()],
                    required: true,
                },
                VerificationCheck {
                    id: "V2".to_string(),
                    command: "true".to_string(),
                    profile: "strict".to_string(),
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

    fn run_git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git command should launch");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git command should launch");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct TempRepo {
        path: std::path::PathBuf,
    }

    impl TempRepo {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn init_temp_git_repo() -> TempRepo {
        let path = std::env::temp_dir().join(format!("athena-test-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp repo dir");
        let repo = path.as_path();
        run_git(repo, &["init"]);
        run_git(repo, &["config", "user.email", "athena-test@example.com"]);
        run_git(repo, &["config", "user.name", "Athena Test"]);
        run_git(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").expect("write base file");
        run_git(repo, &["add", "README.md"]);
        run_git(repo, &["commit", "-m", "init"]);
        TempRepo { path }
    }

    #[tokio::test]
    async fn track_feature_commits_since_filters_to_owned_tagged_commits() {
        let repo_dir = init_temp_git_repo();
        let repo = repo_dir.path();
        let mut last_head = Some(git_stdout(repo, &["rev-parse", "HEAD"]));
        let mut seen = HashSet::new();
        let tag = "athena-feature-run:test-123";

        std::fs::write(repo.join("README.md"), "owned change\n").expect("write owned");
        run_git(repo, &["add", "README.md"]);
        run_git(repo, &["commit", "-m", "owned commit", "-m", tag]);
        let owned_sha = git_stdout(repo, &["rev-parse", "HEAD"]);

        std::fs::write(repo.join("notes.txt"), "unowned\n").expect("write unowned");
        run_git(repo, &["add", "notes.txt"]);
        run_git(repo, &["commit", "-m", "unowned commit"]);
        let unowned_sha = git_stdout(repo, &["rev-parse", "HEAD"]);

        let tracked = track_feature_commits_since(repo, &mut last_head, &mut seen, Some(tag))
            .await
            .expect("track commits");
        assert_eq!(tracked, vec![owned_sha.clone()]);
        assert!(!tracked.contains(&unowned_sha));
        assert!(commit_message_contains_tag(repo, &owned_sha, tag)
            .await
            .expect("owned tag lookup"));
        assert!(!commit_message_contains_tag(repo, &unowned_sha, tag)
            .await
            .expect("unowned tag lookup"));
    }

    #[tokio::test]
    async fn rollback_feature_commits_reverts_owned_commit_without_touching_other_changes() {
        let repo_dir = init_temp_git_repo();
        let repo = repo_dir.path();
        let tag = "athena-feature-run:test-rollback";

        std::fs::write(repo.join("README.md"), "owned delta\n").expect("write owned");
        run_git(repo, &["add", "README.md"]);
        run_git(repo, &["commit", "-m", "owned delta", "-m", tag]);
        let owned_sha = git_stdout(repo, &["rev-parse", "HEAD"]);

        std::fs::write(repo.join("notes.txt"), "keep this\n").expect("write unowned");
        run_git(repo, &["add", "notes.txt"]);
        run_git(repo, &["commit", "-m", "unowned delta"]);

        let rows = rollback_feature_commits(repo, &[owned_sha]).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].reverted);
        assert!(rows[0].error.is_none());
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).expect("read reverted file"),
            "base\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("notes.txt")).expect("read unowned file"),
            "keep this\n"
        );
    }

    #[tokio::test]
    async fn rollback_on_failure_requires_clean_working_tree() {
        let repo_dir = init_temp_git_repo();
        let repo = repo_dir.path();
        ensure_clean_working_tree(repo)
            .await
            .expect("clean repo should pass precheck");

        std::fs::write(repo.join("dirty.txt"), "dirty\n").expect("write dirty file");
        let err = ensure_clean_working_tree(repo)
            .await
            .expect_err("dirty repo should fail precheck");
        let msg = err.to_string();
        assert!(msg.contains("clean working tree"));
    }

    #[test]
    fn rollback_commit_policy_context_includes_tag_and_instruction() {
        let context = append_feature_rollback_commit_policy_context(
            "base context".to_string(),
            "athena-feature-run:test-ctx",
        );
        assert!(context.contains("[feature_rollback_commit_tag:athena-feature-run:test-ctx]"));
        assert!(context.contains("include the rollback tag"));
    }

    #[test]
    fn feature_batch_configured_parallelism_clamps_values() {
        assert_eq!(feature_batch_configured_parallelism_from_raw(None), 2);
        assert_eq!(feature_batch_configured_parallelism_from_raw(Some("0")), 1);
        assert_eq!(feature_batch_configured_parallelism_from_raw(Some("2")), 2);
        assert_eq!(feature_batch_configured_parallelism_from_raw(Some("9")), 4);
        assert_eq!(feature_batch_configured_parallelism_from_raw(Some("bad")), 2);
    }

    #[test]
    fn feature_batch_dynamic_parallelism_high_rss_forces_one() {
        let metrics = SystemMetrics {
            rss_bytes: 9_000,
            total_memory_bytes: 10_000,
            ..SystemMetrics::default()
        };
        let (parallelism, reason) = feature_batch_dynamic_parallelism(4, Some(&metrics));
        assert_eq!(parallelism, 1);
        assert!(reason.contains("rss_pct"));
    }

    #[test]
    fn feature_batch_dynamic_parallelism_high_container_count_forces_one() {
        let metrics = SystemMetrics {
            active_containers: 5,
            ..SystemMetrics::default()
        };
        let (parallelism, reason) = feature_batch_dynamic_parallelism(4, Some(&metrics));
        assert_eq!(parallelism, 1);
        assert!(reason.contains("active_containers"));
    }

    #[test]
    fn feature_batch_dynamic_parallelism_medium_rss_caps_to_two() {
        let metrics = SystemMetrics {
            rss_bytes: 7_000,
            total_memory_bytes: 10_000,
            ..SystemMetrics::default()
        };
        let (parallelism, reason) = feature_batch_dynamic_parallelism(4, Some(&metrics));
        assert_eq!(parallelism, 2);
        assert!(reason.contains("> 60"));
    }

    #[test]
    fn feature_batch_dynamic_parallelism_healthy_uses_configured() {
        let metrics = SystemMetrics {
            rss_bytes: 3_000,
            total_memory_bytes: 10_000,
            ..SystemMetrics::default()
        };
        let (parallelism, reason) = feature_batch_dynamic_parallelism(3, Some(&metrics));
        assert_eq!(parallelism, 3);
        assert!(reason.contains("<= 60"));
    }

    #[test]
    fn feature_batch_dynamic_parallelism_metrics_unavailable_falls_back() {
        let (parallelism, reason) = feature_batch_dynamic_parallelism(3, None);
        assert_eq!(parallelism, 3);
        assert!(reason.contains("unavailable"));
    }

    #[test]
    fn feature_batch_dynamic_parallelism_missing_memory_metrics_falls_back() {
        let metrics = SystemMetrics::default();
        let (parallelism, reason) = feature_batch_dynamic_parallelism(3, Some(&metrics));
        assert_eq!(parallelism, 3);
        assert!(reason.contains("rss_unavailable"));
    }

    #[test]
    fn resolve_self_build_ci_monitor_auto_defaults_on() {
        let (enabled, source) =
            resolve_self_build_ci_monitor(SelfBuildPromoteMode::Auto, false, false);
        assert!(enabled);
        assert_eq!(source, "auto_default");
    }

    #[test]
    fn resolve_self_build_ci_monitor_non_auto_defaults_off() {
        let (enabled, source) = resolve_self_build_ci_monitor(SelfBuildPromoteMode::Pr, false, false);
        assert!(!enabled);
        assert_eq!(source, "default_off");
    }

    #[test]
    fn resolve_self_build_ci_monitor_explicit_on_wins() {
        let (enabled, source) =
            resolve_self_build_ci_monitor(SelfBuildPromoteMode::Auto, true, false);
        assert!(enabled);
        assert_eq!(source, "explicit_on");
    }

    #[test]
    fn resolve_self_build_ci_monitor_explicit_off_wins() {
        let (enabled, source) =
            resolve_self_build_ci_monitor(SelfBuildPromoteMode::Auto, false, true);
        assert!(!enabled);
        assert_eq!(source, "explicit_off");
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
        assert!(ledger.rollback_commits.is_empty());
    }

    #[test]
    fn feature_ledger_markdown_includes_rollback_section_when_present() {
        let contract = sample_contract();
        let mut statuses = HashMap::new();
        statuses.insert("T1".to_string(), FeatureRunStatus::Succeeded);
        statuses.insert("T2".to_string(), FeatureRunStatus::Failed("failed".to_string()));
        let mut ledger = build_feature_run_ledger(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            &statuses,
            &HashMap::new(),
            1,
            1,
            0,
        );
        ledger.rollback_commits = vec![
            super::FeatureRollbackLedgerRow {
                commit_sha: "abc123".to_string(),
                reverted: true,
                error: None,
            },
            super::FeatureRollbackLedgerRow {
                commit_sha: "def456".to_string(),
                reverted: false,
                error: Some("conflict".to_string()),
            },
        ];

        let md = render_feature_ledger_markdown(&ledger);
        assert!(md.contains("## Rollback Commits"));
        assert!(md.contains("abc123"));
        assert!(md.contains("def456"));
        assert!(md.contains("conflict"));
    }

    #[test]
    fn feature_verify_fails_when_required_check_fails() {
        let mut contract = sample_contract();
        contract.verification_checks[1].command = "false".to_string();
        let ledger = run_feature_verify(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            "strict",
        )
        .unwrap();
        assert_eq!(ledger.summary.required_checks_failed, 1);
        assert!(!ledger.summary.promotable);
    }

    #[test]
    fn feature_verify_fails_when_acceptance_has_no_passing_check() {
        let mut contract = sample_contract();
        contract.verification_checks[1].command = "false".to_string();
        let ledger = run_feature_verify(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            "strict",
        )
        .unwrap();
        assert!(!ledger.summary.acceptance_satisfied);
        assert!(ledger
            .summary
            .promotion_reasons
            .iter()
            .any(|r| r.contains("acceptance criteria are not satisfied")));
    }

    #[test]
    fn feature_verify_fast_profile_runs_fast_checks_only() {
        let contract = sample_contract();
        let ledger = run_feature_verify(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            "fast",
        )
        .unwrap();
        assert_eq!(ledger.summary.profile, "fast");
        assert_eq!(ledger.summary.checks_total, 1);
        assert_eq!(ledger.checks[0].check_id, "V1");
        assert_eq!(ledger.checks[0].profile, "fast");
    }

    #[test]
    fn promotion_decision_requires_approval_for_medium_risk() {
        let contract = sample_contract();
        let mut statuses = HashMap::new();
        statuses.insert("T1".to_string(), FeatureRunStatus::Succeeded);
        statuses.insert("T2".to_string(), FeatureRunStatus::Succeeded);
        let dispatch = build_feature_run_ledger(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            &statuses,
            &HashMap::new(),
            2,
            0,
            0,
        );
        let verify = run_feature_verify(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            "strict",
        )
        .unwrap();
        let decision = build_feature_promotion_decision(
            &contract,
            "medium",
            Path::new("eval/feature-contract-example.yaml"),
            Path::new("eval/results/feature-test-dispatch.json"),
            &dispatch,
            Path::new("eval/results/feature-test-verify.json"),
            &verify,
            None,
        );
        assert!(!decision.auto_promotable);
        assert!(decision.approval_required);
        assert!(decision
            .reasons
            .iter()
            .any(|r| r.contains("requires human approval")));
    }

    #[test]
    fn promotion_decision_requires_real_gate_signal_for_auto_promote() {
        let contract = sample_contract();
        let mut statuses = HashMap::new();
        statuses.insert("T1".to_string(), FeatureRunStatus::Succeeded);
        statuses.insert("T2".to_string(), FeatureRunStatus::Succeeded);
        let dispatch = build_feature_run_ledger(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            &statuses,
            &HashMap::new(),
            2,
            0,
            0,
        );
        let verify = run_feature_verify(
            &contract,
            Path::new("eval/feature-contract-example.yaml"),
            "strict",
        )
        .unwrap();
        let decision = build_feature_promotion_decision(
            &contract,
            "low",
            Path::new("eval/feature-contract-example.yaml"),
            Path::new("eval/results/feature-test-dispatch.json"),
            &dispatch,
            Path::new("eval/results/feature-test-verify.json"),
            &verify,
            None,
        );
        assert!(!decision.auto_promotable);
        assert!(decision
            .reasons
            .iter()
            .any(|r| r.contains("no real eval gate result found")));
    }

    #[test]
    fn latest_eval_gate_status_picks_latest_matching_suite() {
        let history =
            std::env::temp_dir().join(format!("athena-history-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(
            &history,
            r#"{"timestamp_utc":"20260217T100000Z","suite":"athena-core-v2-real","gate_ok":false}
{"timestamp_utc":"20260217T090000Z","suite":"other-suite","gate_ok":true}
{"timestamp_utc":"20260217T110000Z","suite":"athena-core-v2-real","gate_ok":true,"report_json":"eval/results/eval-20260217T110000Z.json"}
"#,
        )
        .unwrap();
        let status = latest_eval_gate_status(&history, "athena-core-v2-real")
            .unwrap()
            .unwrap();
        assert_eq!(status.timestamp_utc, "20260217T110000Z");
        assert!(status.gate_ok);
        assert_eq!(
            status.report_json.as_deref(),
            Some("eval/results/eval-20260217T110000Z.json")
        );
        let _ = std::fs::remove_file(&history);
    }

    #[test]
    fn parse_dispatch_task_id_extracts_uuid() {
        let s =
            "Dispatched autonomous task to coder (task_id=123e4567-e89b-12d3-a456-426614174000).";
        assert_eq!(
            parse_dispatch_task_id(s).as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
    }

    #[test]
    fn parse_dispatch_task_id_picks_first_match() {
        let s = "task_id=11111111-1111-1111-1111-111111111111 ... task_id=22222222-2222-2222-2222-222222222222";
        assert_eq!(
            parse_dispatch_task_id(s).as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
    }

    #[test]
    fn parse_dispatch_task_id_rejects_malformed_uuid() {
        let s = "Dispatched task_id=not-a-uuid value.";
        assert!(parse_dispatch_task_id(s).is_none());
    }

    #[test]
    fn parse_dispatch_task_id_returns_none_when_missing() {
        let s = "Dispatched autonomous task without id.";
        assert!(parse_dispatch_task_id(s).is_none());
    }

    #[test]
    fn parse_git_status_paths_supports_rename_line() {
        let out = " M src/main.rs\nR  old.txt -> new.txt\n?? notes.md\n";
        let paths = parse_git_status_paths(out);
        assert_eq!(paths, vec!["new.txt", "notes.md", "src/main.rs"]);
    }

    #[test]
    fn self_build_guardrails_detect_secret_addition() {
        let diff = r#"
diff --git a/config.toml b/config.toml
index 1111111..2222222 100644
--- a/config.toml
+++ b/config.toml
@@ -1,3 +1,4 @@
+github_token = "ghp_abcdefghijklmnopqrstuvwxyz123456"
"#;
        let report = evaluate_self_build_guardrails(&["config.toml".to_string()], diff, "", "");
        assert!(!report.passed);
        assert!(report
            .violations
            .iter()
            .any(|v| v.contains("secret-like material")));
    }

    #[test]
    fn promotion_plan_auto_low_can_merge() {
        let guardrails = evaluate_self_build_guardrails(&[], "", "", "");
        let plan = plan_self_build_promotion(
            SelfBuildPromoteMode::Auto,
            "low",
            true,
            true,
            true,
            &guardrails,
            true,
        );
        assert_eq!(plan.status, "ready_merge");
        assert!(plan.open_pr);
        assert!(plan.merge_pr);
    }

    #[test]
    fn promotion_plan_auto_medium_is_pr_only() {
        let guardrails = evaluate_self_build_guardrails(&[], "", "", "");
        let plan = plan_self_build_promotion(
            SelfBuildPromoteMode::Auto,
            "medium",
            true,
            true,
            true,
            &guardrails,
            true,
        );
        assert_eq!(plan.status, "ready_pr");
        assert!(plan.open_pr);
        assert!(!plan.merge_pr);
    }

    #[test]
    fn promotion_plan_blocks_without_critic_pass() {
        let guardrails = evaluate_self_build_guardrails(&[], "", "", "");
        let plan = plan_self_build_promotion(
            SelfBuildPromoteMode::Pr,
            "low",
            true,
            false,
            true,
            &guardrails,
            true,
        );
        assert_eq!(plan.status, "blocked");
        assert!(!plan.open_pr);
        assert!(!plan.merge_pr);
    }

    #[test]
    fn promotion_plan_blocks_on_hard_guardrail_codes() {
        let guardrails = evaluate_self_build_guardrails(
            &[".env".to_string()],
            "+token=\"abc123456789\"",
            "git reset --hard",
            "",
        );
        let plan = plan_self_build_promotion(
            SelfBuildPromoteMode::Pr,
            "low",
            true,
            true,
            true,
            &guardrails,
            true,
        );
        assert_eq!(plan.status, "blocked");
        assert!(plan.reasons.iter().any(|r| r.contains("policy.guardrail.")));
    }

    #[test]
    fn review_checklist_marks_merge_not_ready_on_guardrail_failure() {
        let guardrails = evaluate_self_build_guardrails(
            &[".env".to_string()],
            "+token=\"abc123456789\"",
            "",
            "",
        );
        let critic = super::SelfBuildCriticReport {
            score: 0.92,
            passed: true,
            reasons: Vec::new(),
        };
        let checklist = build_self_build_review_checklist(
            "run-x",
            "low",
            "Rotate token handling",
            &[".env".to_string()],
            &["10\t1\t.env".to_string()],
            &guardrails,
            &critic,
            &[],
        );
        assert!(!checklist.merge_ready);
        assert!(!checklist.blockers.is_empty());
    }

    #[tokio::test]
    async fn wait_for_autonomous_pulse_ignores_non_autonomous_sources_with_same_task_id() {
        // Regression: ensure pulses from non-AutonomousTask sources are never
        // treated as dispatch results, even when their task_id matches.
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mut rx = tx.subscribe();

        // Heartbeat and Scheduler pulses with the target task_id must be ignored.
        let _ = tx.send(
            Pulse::new(PulseSource::Heartbeat, Urgency::Low, "heartbeat".into())
                .with_task_id("dispatch-42"),
        );
        let _ = tx.send(
            Pulse::new(
                PulseSource::CronJob("test".into()),
                Urgency::Medium,
                "scheduled".into(),
            )
            .with_task_id("dispatch-42"),
        );
        // Wrong task_id from autonomous source must also be ignored.
        let _ = tx.send(
            Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "wrong".into())
                .with_task_id("dispatch-99"),
        );
        // Correct source + correct task_id must resolve.
        let _ = tx.send(
            Pulse::new(PulseSource::AutonomousTask, Urgency::Medium, "done".into())
                .with_task_id("dispatch-42"),
        );

        let res = wait_for_autonomous_pulse(&mut rx, "dispatch-42", 2).await;
        assert_eq!(res, WaitForAutonomousOutcome::Received("done".to_string()));
    }

    #[test]
    fn feature_task_context_includes_predecessor_result_summaries() {
        let contract = sample_contract();
        let task = contract.task_by_id("T2").expect("T2 task missing");
        let mut predecessor_summaries = HashMap::new();
        predecessor_summaries.insert(
            "T1".to_string(),
            "Parser and validation implemented with tests".to_string(),
        );

        let context = build_feature_task_context(&contract, task, &predecessor_summaries);
        assert!(context.contains("Previous task results:"));
        assert!(context.contains("- T1: Parser and validation implemented with tests"));
    }

    #[test]
    fn feature_task_context_truncates_predecessor_result_summaries_to_500_chars() {
        let contract = sample_contract();
        let task = contract.task_by_id("T2").expect("T2 task missing");
        let mut predecessor_summaries = HashMap::new();
        predecessor_summaries.insert("T1".to_string(), "x".repeat(700));

        let context = build_feature_task_context(&contract, task, &predecessor_summaries);
        let prefix = "- T1: ";
        let line = context
            .lines()
            .find(|line| line.starts_with(prefix))
            .expect("expected predecessor summary line");
        let summary = &line[prefix.len()..];
        assert_eq!(summary.chars().count(), 500);
    }

    #[test]
    fn feature_task_context_includes_only_direct_predecessor_summaries() {
        let mut contract = sample_contract();
        contract.tasks.push(FeatureTask {
            id: "T3".to_string(),
            goal: "task3".to_string(),
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
            depends_on: vec!["T2".to_string()],
            enabled: true,
        });
        let task = contract.task_by_id("T3").expect("T3 task missing");
        let mut predecessor_summaries = HashMap::new();
        predecessor_summaries.insert("T1".to_string(), "summary from T1".to_string());
        predecessor_summaries.insert("T2".to_string(), "summary from T2".to_string());

        let context = build_feature_task_context(&contract, task, &predecessor_summaries);
        assert!(context.contains("Previous task results:"));
        assert!(context.contains("- T2: summary from T2"));
        assert!(!context.contains("- T1: summary from T1"));
    }

    #[test]
    fn feature_task_context_omits_previous_results_when_summary_missing_or_empty() {
        let contract = sample_contract();
        let task = contract.task_by_id("T2").expect("T2 task missing");

        let context_without_summary = build_feature_task_context(&contract, task, &HashMap::new());
        assert!(!context_without_summary.contains("Previous task results:"));

        let mut predecessor_summaries = HashMap::new();
        predecessor_summaries.insert("T1".to_string(), "   \n\t".to_string());
        let context_with_empty_summary =
            build_feature_task_context(&contract, task, &predecessor_summaries);
        assert!(!context_with_empty_summary.contains("Previous task results:"));
    }

    #[test]
    fn outcome_grace_override_takes_precedence() {
        let task = sample_contract().tasks[0].clone();
        let secs = compute_feature_outcome_grace_secs(&task, "delivery", "low", 30, Some(42));
        assert_eq!(secs, 42);
    }

    #[test]
    fn outcome_grace_scales_with_task_profile() {
        let mut task = sample_contract().tasks[0].clone();
        task.ghost = Some("coder".to_string());
        task.goal = "Implement integration test coverage".to_string();
        let secs = compute_feature_outcome_grace_secs(&task, "delivery", "low", 60, None);
        assert!(secs >= 360);
    }
}
