mod config;
mod confirm;
mod core;
mod db;
mod docker;
mod error;
mod executor;
mod llm;
mod manager;
mod memory;
mod profiles;
mod strategy;
mod tools;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use confirm::CliConfirmer;
use config::Config;
use core::{AthenaCore, CoreEvent, SessionContext};
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
    /// List configured agents
    Agents,
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
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("athena=info".parse().unwrap()),
        )
        .with_target(false)
        .with_ansi(atty::is(atty::Stream::Stderr))
        .compact()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let auto_approve = cli.auto_approve || !atty::is(atty::Stream::Stdin);
    let config = Config::load(cli.config.as_deref())?;

    // Initialize database
    let db_path = config.db_path()?;
    let conn = db::init_db(&db_path)?;
    let memory = Arc::new(MemoryStore::new(conn));

    match cli.command {
        Some(Commands::Memory { action }) => handle_memory(action, &memory)?,
        Some(Commands::Agents) => {
            // Start core to get merged agent list (config + profiles)
            let handle = AthenaCore::start(config, memory).await?;
            for a in handle.list_agents() {
                println!("  {} — {} [{}]", a.name, a.description, a.tools.join(", "));
            }
        }
        Some(Commands::Chat) | None => run_chat(config, memory, auto_approve).await?,
    }

    Ok(())
}

fn handle_memory(action: MemoryAction, memory: &MemoryStore) -> anyhow::Result<()> {
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
            let id = memory.store(&category, &content)?;
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
                println!("  /agents    — List configured agents");
                println!("  /memories  — List saved memories");
                println!("  /help      — This help");
                println!("  /quit      — Exit");
                continue;
            }
            "/agents" => {
                for a in handle.list_agents() {
                    println!(
                        "  {} — {} [{}]",
                        a.name,
                        a.description,
                        a.tools.join(", ")
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
    eprintln!("Goodbye.");
    Ok(())
}
