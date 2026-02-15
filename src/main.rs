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
mod heartbeat;
mod introspect;
mod kpi;
mod knobs;
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
            wait_secs,
            lane,
            risk,
            repo,
        }) => {
            run_dispatch(
                config, memory, goal, context, ghost, auto_store, wait_secs, lane, risk, repo,
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
                &format!("dispatch_wait_timeout_after={}s", wait_secs),
            );
            Ok(())
        }
        WaitForAutonomousOutcome::ChannelClosed => {
            mark_dispatch_task_failed_if_started(
                &config_for_finalize,
                &task_id,
                "dispatch_wait_channel_closed",
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
        classify_chat_command, pulse_matches_task_id, wait_for_autonomous_pulse, ChatCommand,
        WaitForAutonomousOutcome,
    };
    use crate::pulse::{Pulse, PulseSource, Urgency};

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
        assert_eq!(classify_chat_command("/set temperature 0.2"), ChatCommand::Set);
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
}
