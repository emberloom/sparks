#![allow(dead_code)]

mod config;
mod confirm;
mod core;
mod db;
mod docker;
mod dynamic_tools;
mod embeddings;
mod error;
mod executor;
mod heartbeat;
mod knobs;
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
mod tools;

use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use confirm::CliConfirmer;
use config::Config;
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

    // Initialize embedder for CLI memory commands (lightweight, no LLM needed)
    let embedder = if config.embedding.enabled {
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

    // Backfill any memories missing embeddings (runs on every CLI invocation, fast no-op if none)
    if let Some(ref e) = embedder {
        core::backfill_embeddings(&memory, e);
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
                    "openrouter" => config.openrouter.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
                    "zen" => config.zen.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
                    _ => config.ollama.model.clone(),
                },
                temperature: match config.llm.provider.as_str() {
                    "openrouter" => config.openrouter.as_ref().map(|c| c.temperature).unwrap_or(0.3),
                    "zen" => config.zen.as_ref().map(|c| c.temperature).unwrap_or(0.3),
                    _ => config.ollama.temperature,
                },
                max_tokens: match config.llm.provider.as_str() {
                    "openrouter" => config.openrouter.as_ref().map(|c| c.max_tokens).unwrap_or(4096),
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
        Some(Commands::Chat) | None => run_chat(config, memory, auto_approve).await?,
    }

    Ok(())
}

fn handle_memory(action: MemoryAction, memory: &MemoryStore, embedder: Option<&Embedder>) -> anyhow::Result<()> {
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
    let engine = handle.cron_engine.as_ref().expect("Cron engine not initialized");
    match action {
        JobsAction::List => {
            let jobs = engine.list_jobs()?;
            if jobs.is_empty() {
                println!("No scheduled jobs.");
            } else {
                for j in &jobs {
                    let status = if j.enabled { "on" } else { "off" };
                    let next = j.next_run.map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "-".to_string());
                    println!("  [{}] {} ({}) — next: {} — {}", &j.id[..8], j.name, status, next, j.prompt);
                }
            }
        }
        JobsAction::Add { name, every, cron, prompt } => {
            let schedule = if let Some(secs) = every {
                Schedule::Interval { every_secs: secs, jitter: 0.1 }
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
            let full_id = jobs.iter().find(|j| j.id.starts_with(&id)).map(|j| j.id.clone());
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

    let history_path = dirs::home_dir()
        .map(|h| h.join(".athena").join("history.txt"))
        .unwrap_or_else(|| PathBuf::from(".athena_history"));

    let mut rl = rustyline::DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    // Spawn background task to print delivered pulses
    let delivered_rx = handle.delivered_rx.clone();
    tokio::spawn(async move {
        let mut rx = delivered_rx.lock().await;
        while let Some(pulse) = rx.recv().await {
            eprintln!("\n\x1b[2;36m[{}] {}\x1b[0m", pulse.source.label(), pulse.content);
            eprint!("you> "); // re-show prompt
        }
    });

    loop {
        let line = match rl.readline("you> ") {
            Ok(line) => line,
            Err(
                rustyline::error::ReadlineError::Interrupted
                | rustyline::error::ReadlineError::Eof,
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

        // Slash commands
        if input.starts_with("/set") {
            let parts: Vec<&str> = input.split_whitespace().collect();
            match parts.len() {
                1 => {
                    // /set — show all knobs
                    let k = handle.knobs.read().unwrap();
                    println!("{}", k.display());
                }
                3 => {
                    // /set key value
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
            continue;
        }

        match input {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("Commands:");
                println!("  /ghosts    — List active ghosts");
                println!("  /memories  — List saved memories");
                println!("  /set       — Show/change runtime knobs");
                println!("  /mood      — Show current mood");
                println!("  /jobs      — List scheduled jobs");
                println!("  /help      — This help");
                println!("  /quit      — Exit");
                continue;
            }
            "/ghosts" => {
                for g in handle.list_ghosts() {
                    println!(
                        "  {} — {} [{}]",
                        g.name,
                        g.description,
                        g.tools.join(", ")
                    );
                }
                continue;
            }
            "/memories" => {
                match handle.list_memories() {
                    Ok(memories) if memories.is_empty() => println!("No memories."),
                    Ok(memories) => {
                        for m in &memories {
                            println!("  [{}] {} — {}", &m.id[..8], m.category, m.content);
                        }
                    }
                    Err(e) => eprintln!("Error: {}", e),
                }
                continue;
            }
            "/mood" => {
                println!("{}", handle.mood.describe());
                continue;
            }
            "/jobs" => {
                if let Some(engine) = &handle.cron_engine {
                    match engine.list_jobs() {
                        Ok(jobs) if jobs.is_empty() => println!("No scheduled jobs."),
                        Ok(jobs) => {
                            for j in &jobs {
                                let status = if j.enabled { "on" } else { "off" };
                                let next = j.next_run.map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "-".to_string());
                                println!("  [{}] {} ({}) next: {}", &j.id[..8], j.name, status, next);
                            }
                        }
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                continue;
            }
            _ => {}
        }

        // Send through core
        let mut events = match handle
            .chat(session.clone(), input, confirmer.clone())
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };

        while let Some(event) = events.recv().await {
            match event {
                CoreEvent::Status(s) => eprintln!("  {}", s),
                CoreEvent::Response(r) => println!("\n{}\n", r),
                CoreEvent::Error(e) => {
                    if e.contains("cancelled") {
                        println!("Action cancelled.");
                    } else {
                        eprintln!("Error: {}", e);
                    }
                }
                CoreEvent::Pulse(p) => {
                    println!("\n[pulse] {}\n", p);
                }
            }
        }
    }

    let _ = rl.save_history(&history_path);

    // L7: Restrict history file permissions to owner-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if history_path.exists() {
            let _ = std::fs::set_permissions(
                &history_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }

    eprintln!("Goodbye.");
    Ok(())
}
