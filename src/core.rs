use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::confirm::Confirmer;
use crate::error::Result;
use crate::llm::OllamaClient;
use crate::manager::Manager;
use crate::memory::MemoryStore;
use crate::profiles;

/// Identifies who is talking — scopes memory and conversation.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub platform: String,
    pub user_id: String,
    pub chat_id: String,
}

/// Progressive updates from core to frontend.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// Agent is working on something
    Status(String),
    /// Final complete response
    Response(String),
    /// Error during execution
    Error(String),
}

/// Request from any frontend to the core.
struct CoreRequest {
    session: SessionContext,
    input: String,
    confirmer: Arc<dyn Confirmer>,
    event_tx: mpsc::Sender<CoreEvent>,
}

/// Info about a configured agent (returned by list_agents).
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub strategy: String,
}

/// Info about a stored memory (returned by list_memories).
#[derive(Debug, Clone)]
pub struct MemoryInfo {
    pub id: String,
    pub category: String,
    pub content: String,
}

/// Opaque, clonable handle — frontends never touch Manager or Memory directly.
#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreRequest>,
    agents: Arc<Vec<AgentInfo>>,
    memory: Arc<MemoryStore>,
}

impl CoreHandle {
    /// Send a chat message, returns a receiver for streaming events.
    pub async fn chat(
        &self,
        session: SessionContext,
        input: &str,
        confirmer: Arc<dyn Confirmer>,
    ) -> Result<mpsc::Receiver<CoreEvent>> {
        let (event_tx, event_rx) = mpsc::channel(32);
        let req = CoreRequest {
            session,
            input: input.to_string(),
            confirmer,
            event_tx,
        };
        self.tx.send(req).await.map_err(|_| {
            crate::error::AthenaError::Tool("Core task has shut down".into())
        })?;
        Ok(event_rx)
    }

    pub fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents.as_ref().clone()
    }

    pub fn list_memories(&self) -> Result<Vec<MemoryInfo>> {
        let memories = self.memory.list()?;
        Ok(memories
            .into_iter()
            .map(|m| MemoryInfo {
                id: m.id,
                category: m.category,
                content: m.content,
            })
            .collect())
    }
}

/// The core engine. Owns Manager and Memory, runs as a tokio task.
pub struct AthenaCore;

impl AthenaCore {
    pub async fn start(config: Config, memory: Arc<MemoryStore>) -> Result<CoreHandle> {
        let llm = OllamaClient::new(config.ollama.clone());

        // Health check
        eprint!("Connecting to Ollama... ");
        match llm.health_check().await {
            Ok(()) => eprintln!("ok (model: {})", config.ollama.model),
            Err(e) => {
                eprintln!("failed: {}", e);
                return Err(e);
            }
        }

        // Merge config agents with ~/.athena/agents/*.toml profiles
        let merged_agents = profiles::load_agents(&config)?;

        let agents: Vec<AgentInfo> = merged_agents
            .iter()
            .map(|a| AgentInfo {
                name: a.name.clone(),
                description: a.description.clone(),
                tools: a.tools.clone(),
                strategy: a.strategy.clone(),
            })
            .collect();

        let manager = Arc::new(Manager::new(&config, merged_agents, llm, memory.clone()));
        let (tx, mut rx) = mpsc::channel::<CoreRequest>(32);

        // Spawn the core event loop
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let manager = manager.clone();
                tokio::spawn(async move {
                    let _ = req
                        .event_tx
                        .send(CoreEvent::Status("Thinking...".into()))
                        .await;

                    match manager
                        .handle(&req.input, &req.session, req.confirmer.as_ref())
                        .await
                    {
                        Ok(response) => {
                            let _ = req.event_tx.send(CoreEvent::Response(response)).await;
                        }
                        Err(e) => {
                            let _ = req
                                .event_tx
                                .send(CoreEvent::Error(e.to_string()))
                                .await;
                        }
                    }
                });
            }
        });

        Ok(CoreHandle {
            tx,
            agents: Arc::new(agents),
            memory,
        })
    }
}
