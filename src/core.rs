use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::confirm::Confirmer;
use crate::embeddings::Embedder;
use crate::error::Result;
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

impl SessionContext {
    pub fn session_key(&self) -> String {
        format!("{}:{}:{}", self.platform, self.user_id, self.chat_id)
    }
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

/// Info about a configured ghost (returned by list_ghosts).
#[derive(Debug, Clone)]
pub struct GhostInfo {
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
    ghosts: Arc<Vec<GhostInfo>>,
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

    pub fn list_ghosts(&self) -> Vec<GhostInfo> {
        self.ghosts.as_ref().clone()
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
        let llm = config.build_llm_provider()?;
        let classifier = config.build_classifier_provider(&llm)?;

        // Health check
        eprint!("Connecting to {}... ", llm.provider_name());
        match llm.health_check().await {
            Ok(()) => eprintln!("ok"),
            Err(e) => {
                eprintln!("failed: {}", e);
                return Err(e);
            }
        }
        if classifier.provider_name() != llm.provider_name() {
            eprint!("Connecting to {}... ", classifier.provider_name());
            match classifier.health_check().await {
                Ok(()) => eprintln!("ok"),
                Err(e) => {
                    eprintln!("failed: {}", e);
                    return Err(e);
                }
            }
        }

        // Initialize embedding model on blocking thread (ensure_model may download via reqwest::blocking)
        let embedder: Option<Arc<Embedder>> = if config.embedding.enabled {
            let cfg = config.clone();
            match tokio::task::spawn_blocking(move || init_embedder(&cfg)).await {
                Ok(Ok(e)) => Some(Arc::new(e)),
                Ok(Err(e)) => {
                    tracing::warn!("Embedding model unavailable, falling back to keyword search: {}", e);
                    None
                }
                Err(e) => {
                    tracing::warn!("Embedder init task panicked: {}", e);
                    None
                }
            }
        } else {
            tracing::info!("Embedding model disabled in config");
            None
        };

        // Spawn background backfill of existing memories without embeddings
        if let Some(ref embedder) = embedder {
            let embedder = embedder.clone();
            let memory_for_backfill = memory.clone();
            tokio::task::spawn_blocking(move || {
                backfill_embeddings(&memory_for_backfill, &embedder);
            });
        }

        // Merge config ghosts with ~/.athena/ghosts/*.toml profiles
        let merged_ghosts = profiles::load_ghosts(&config)?;

        let ghosts: Vec<GhostInfo> = merged_ghosts
            .iter()
            .map(|g| GhostInfo {
                name: g.name.clone(),
                description: g.description.clone(),
                tools: g.tools.clone(),
                strategy: g.strategy.clone(),
            })
            .collect();

        // Spawn periodic conversation cleanup
        {
            let memory_for_cleanup = memory.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
                loop {
                    interval.tick().await;
                    if let Ok(n) = memory_for_cleanup.cleanup_conversations(7) {
                        if n > 0 {
                            tracing::info!("Cleaned up {} old conversation turns", n);
                        }
                    }
                }
            });
        }

        let persona_soul = config.persona.soul.clone();
        let manager = Arc::new(Manager::new(
            &config, merged_ghosts, llm, classifier, memory.clone(), embedder, persona_soul,
        ));
        let (tx, mut rx) = mpsc::channel::<CoreRequest>(32);

        // Spawn the core event loop
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                tracing::debug!(input = %req.input, "Core received request");
                let manager = manager.clone();
                tokio::spawn(async move {
                    let _ = req
                        .event_tx
                        .send(CoreEvent::Status("Thinking...".into()))
                        .await;

                    tracing::debug!("Calling manager.handle()");
                    match manager
                        .handle(&req.input, &req.session, req.confirmer.as_ref())
                        .await
                    {
                        Ok(response) => {
                            tracing::debug!(len = response.len(), "Manager returned response");
                            let _ = req.event_tx.send(CoreEvent::Response(response)).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Manager returned error");
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
            ghosts: Arc::new(ghosts),
            memory,
        })
    }
}

fn init_embedder(config: &Config) -> Result<Embedder> {
    let model_dir = config.resolve_model_dir()?;
    Embedder::ensure_model(&model_dir)?;
    Embedder::new(&model_dir)
}

pub fn backfill_embeddings(memory: &MemoryStore, embedder: &Embedder) {
    let missing = match memory.memories_without_embeddings() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Failed to query memories for backfill: {}", e);
            return;
        }
    };

    if missing.is_empty() {
        return;
    }

    tracing::info!("Backfilling embeddings for {} memories", missing.len());
    let mut done = 0;
    for (id, content) in &missing {
        match embedder.embed(content) {
            Ok(vec) => {
                if let Err(e) = memory.backfill_embedding(id, &vec) {
                    tracing::warn!("Failed to backfill memory {}: {}", &id[..8], e);
                }
                done += 1;
            }
            Err(e) => {
                tracing::warn!("Failed to embed memory {}: {}", &id[..8], e);
            }
        }
    }
    tracing::info!("Backfilled {}/{} memory embeddings", done, missing.len());
}
