mod config;
mod confirm;
mod core;
mod db;
mod docker;
mod embeddings;
mod error;
mod executor;
mod llm;
mod manager;
mod memory;
mod profiles;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "athena=info".parse().unwrap()),
        )
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .compact()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

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
            let handle = AthenaCore::start(config.clone(), memory).await?;
            telegram::run_telegram(handle, config.telegram).await?;
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
        match input {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("Commands:");
                println!("  /ghosts    — List active ghosts");
                println!("  /memories  — List saved memories");
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
