mod config;
mod confirm;
mod db;
mod docker;
mod error;
mod executor;
mod llm;
mod manager;
mod memory;
mod strategy;
mod tools;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use llm::OllamaClient;
use manager::Manager;
use memory::MemoryStore;

#[derive(Parser)]
#[command(name = "athena", about = "Secure autonomous multi-agent system")]
struct Cli {
    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

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
        .init();

    let cli = Cli::parse();

    let config = Config::load(cli.config.as_deref())?;

    // Initialize database
    let db_path = config.db_path()?;
    let conn = db::init_db(&db_path)?;
    let memory = Arc::new(MemoryStore::new(conn));

    match cli.command {
        Some(Commands::Memory { action }) => handle_memory(action, &memory)?,
        Some(Commands::Agents) => handle_agents(&config),
        Some(Commands::Chat) | None => run_chat(config, memory).await?,
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
            // Support short IDs
            let memories = memory.list()?;
            let full_id = memories.iter()
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

fn handle_agents(config: &Config) {
    println!("Configured agents:\n");
    for agent in &config.agents {
        println!("  {} — {}", agent.name, agent.description);
        println!("    Tools: {}", agent.tools.join(", "));
        println!("    Strategy: {}", agent.strategy);
        for m in &agent.mounts {
            println!("    Mount: {} → {} ({})",
                m.host_path, m.container_path,
                if m.read_only { "ro" } else { "rw" });
        }
        println!();
    }
}

async fn run_chat(config: Config, memory: Arc<MemoryStore>) -> anyhow::Result<()> {
    let llm = OllamaClient::new(config.ollama.clone());

    // Health check
    eprint!("Connecting to Ollama... ");
    match llm.health_check().await {
        Ok(()) => eprintln!("ok (model: {})", config.ollama.model),
        Err(e) => {
            eprintln!("failed: {}", e);
            return Err(e.into());
        }
    }

    let manager = Manager::new(&config, llm, memory.clone());

    eprintln!("Athena ready. Type /help for commands.\n");

    let history_path = dirs::home_dir()
        .map(|h| h.join(".athena").join("history.txt"))
        .unwrap_or_else(|| PathBuf::from(".athena_history"));

    let mut rl = rustyline::DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    loop {
        let line = match rl.readline("you> ") {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof) => {
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
                for a in &config.agents {
                    println!("  {} — {} [{}]", a.name, a.description, a.tools.join(", "));
                }
                continue;
            }
            "/memories" => {
                let memories = memory.list()?;
                if memories.is_empty() {
                    println!("No memories.");
                } else {
                    for m in &memories {
                        println!("  [{}] {} — {}", &m.id[..8], m.category, m.content);
                    }
                }
                continue;
            }
            _ => {}
        }

        // Handle via manager
        match manager.handle(input).await {
            Ok(response) => {
                println!("\n{}\n", response);
            }
            Err(error::AthenaError::Cancelled) => {
                println!("Action cancelled.");
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }

    let _ = rl.save_history(&history_path);
    eprintln!("Goodbye.");
    Ok(())
}
