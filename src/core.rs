use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::config::Config;
use crate::confirm::{AutoConfirmer, Confirmer};
use crate::embeddings::Embedder;
use crate::error::Result;
use crate::heartbeat;
use crate::knobs::{RuntimeKnobs, SharedKnobs};
use crate::manager::Manager;
use crate::memory::MemoryStore;
use crate::mood::MoodState;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::proactive::{self, ActivityTracker};
use crate::profiles;
use crate::pulse::{self, Pulse, PulseBus};
use crate::randomness;
use crate::scheduler::CronEngine;

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
    /// Proactive pulse from background tasks
    Pulse(String),
}

/// An autonomous task submitted by a background process (cron, heartbeat, etc.).
#[derive(Debug, Clone)]
pub struct AutonomousTask {
    /// What to accomplish
    pub goal: String,
    /// Background context for the ghost
    pub context: String,
    /// Specific ghost to use (None = let orchestrator classify)
    pub ghost: Option<String>,
    /// Where to deliver the result (Broadcast if not specified)
    pub target: crate::pulse::PulseTarget,
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
    pub memory: Arc<MemoryStore>,
    pub knobs: SharedKnobs,
    pub observer: ObserverHandle,
    pub pulse_bus: PulseBus,
    pub activity: Arc<ActivityTracker>,
    pub mood: Arc<MoodState>,
    pub cron_engine: Option<Arc<CronEngine>>,
    pub delivered_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Pulse>>>,
    pub auto_tx: mpsc::Sender<AutonomousTask>,
}

impl CoreHandle {
    /// Submit an autonomous task for background execution by a ghost.
    /// Results are delivered as pulses to the specified target.
    pub async fn dispatch_task(&self, task: AutonomousTask) -> Result<()> {
        self.auto_tx.send(task).await.map_err(|_| {
            crate::error::AthenaError::Tool("Autonomous task queue full or shut down".into())
        })?;
        Ok(())
    }

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
        let orchestrator = config.build_orchestrator_provider(&llm)?;

        // Health check
        eprint!("Connecting to {}... ", llm.provider_name());
        match llm.health_check().await {
            Ok(()) => eprintln!("ok"),
            Err(e) => {
                eprintln!("failed: {}", e);
                return Err(e);
            }
        }
        if orchestrator.provider_name() != llm.provider_name() {
            eprint!("Connecting to {}... ", orchestrator.provider_name());
            match orchestrator.health_check().await {
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

        // --- Initialize new subsystems ---

        // Observer
        let observer = ObserverHandle::new(1024);
        crate::observer::spawn_uds_listener(observer.clone());
        observer.log(ObserverCategory::Startup, "Athena core started, observer active");

        // Runtime knobs
        let knobs: SharedKnobs = Arc::new(std::sync::RwLock::new(RuntimeKnobs::from_config(&config)));

        // Mood state
        let mood = Arc::new(MoodState::load(&memory, config.mood.timezone_offset));

        // Pulse bus + consumer
        let pulse_bus = PulseBus::new(256);
        let (delivered_tx, delivered_rx) = mpsc::channel::<Pulse>(64);
        pulse::spawn_pulse_consumer(pulse_bus.clone(), observer.clone(), delivered_tx, knobs.clone());

        // Activity tracker
        let activity = Arc::new(ActivityTracker::new());

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

        // Spawn mood drift loop
        {
            let mood = mood.clone();
            let knobs = knobs.clone();
            let observer = observer.clone();
            let memory_for_mood = memory.clone();
            tokio::spawn(async move {
                loop {
                    let (interval, enabled, all) = {
                        let k = knobs.read().unwrap();
                        (k.mood_drift_interval_secs, k.mood_enabled, k.all_proactive)
                    };
                    if !all || !enabled {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    let dur = randomness::jitter_interval(interval, 0.2);
                    tokio::time::sleep(dur).await;
                    {
                        let k = knobs.read().unwrap();
                        if !k.all_proactive || !k.mood_enabled {
                            continue;
                        }
                    }
                    mood.drift(&observer);
                    mood.save(&memory_for_mood);
                }
            });
        }

        // Spawn heartbeat loop
        heartbeat::spawn_heartbeat_loop(
            knobs.clone(),
            observer.clone(),
            pulse_bus.clone(),
            llm.clone(),
            memory.clone(),
            mood.clone(),
            config.heartbeat.soul_file.clone(),
        );

        // Spawn memory scanner
        proactive::spawn_memory_scanner(
            knobs.clone(),
            observer.clone(),
            pulse_bus.clone(),
            llm.clone(),
            memory.clone(),
        );

        // Spawn idle musings
        proactive::spawn_idle_musings(
            knobs.clone(),
            observer.clone(),
            pulse_bus.clone(),
            llm.clone(),
            memory.clone(),
            activity.clone(),
        );

        // Cron engine
        let cron_engine = Arc::new(CronEngine::new(
            memory.clone(),
            observer.clone(),
            pulse_bus.clone(),
            llm.clone(),
            knobs.clone(),
        ));
        cron_engine.clone().spawn_tick_loop();

        // Autonomous task channel — created early so background tasks can receive auto_tx
        let (auto_tx, mut auto_rx) = mpsc::channel::<AutonomousTask>(32);

        let persona_soul = config.persona.soul.clone();
        let persona_soul_for_reentry = persona_soul.clone();
        let self_knowledge = config.persona.self_knowledge.clone();
        let tools_doc = config.persona.tools_doc.clone();
        let manager = Arc::new(Manager::new(
            &config, merged_ghosts, llm, orchestrator, memory.clone(), embedder, persona_soul,
            self_knowledge, tools_doc, mood.clone(), knobs.clone(),
        ));
        let (tx, mut rx) = mpsc::channel::<CoreRequest>(32);

        // Spawn the core event loop
        let manager_for_auto = manager.clone(); // clone before moving into event loop
        let pulse_bus_for_reentry = pulse_bus.clone();
        let knobs_for_reentry = knobs.clone();
        let observer_for_reentry = observer.clone();
        let memory_for_reentry = memory.clone();
        let llm_for_reentry = manager.llm_ref();
        let activity_for_events = activity.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                tracing::debug!(input = %req.input, "Core received request");
                let manager = manager.clone();
                let activity = activity_for_events.clone();
                let knobs = knobs_for_reentry.clone();
                let observer = observer_for_reentry.clone();
                let pulse_bus = pulse_bus_for_reentry.clone();
                let memory = memory_for_reentry.clone();
                let llm = llm_for_reentry.clone();
                let persona_soul = persona_soul_for_reentry.clone();
                let session_key = req.session.session_key();
                let request_id = uuid::Uuid::new_v4();
                let request_span = tracing::info_span!(
                    "request",
                    id = %request_id,
                );
                tokio::spawn(async move {
                    activity.touch();

                    observer.emit(
                        crate::observer::ObserverEvent::new(
                            ObserverCategory::ChatIn,
                            format!("{} \"{}\"", session_key, truncate_obs(&req.input, 80)),
                        )
                    );

                    let _ = req
                        .event_tx
                        .send(CoreEvent::Status("Thinking...".into()))
                        .await;

                    // Create a status bridge: strategy sends String → core maps to CoreEvent::Status
                    let (status_tx, mut status_rx) = mpsc::channel::<String>(16);
                    let event_tx_for_status = req.event_tx.clone();
                    tokio::spawn(async move {
                        while let Some(msg) = status_rx.recv().await {
                            let _ = event_tx_for_status
                                .send(CoreEvent::Status(msg))
                                .await;
                        }
                    });

                    tracing::debug!("Calling manager.handle()");
                    match manager
                        .handle(&req.input, &req.session, req.confirmer.as_ref(), Some(&status_tx))
                        .await
                    {
                        Ok(response) => {
                            tracing::debug!(len = response.len(), "Manager returned response");
                            observer.emit(
                                crate::observer::ObserverEvent::new(
                                    ObserverCategory::ChatOut,
                                    format!("{} ({} chars)", session_key, response.len()),
                                ).with_details(truncate_obs(&response, 100))
                            );
                            let _ = req.event_tx.send(CoreEvent::Response(response)).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Manager returned error");
                            observer.log(ObserverCategory::ChatOut, format!("{} ERROR: {}", session_key, e));
                            let _ = req
                                .event_tx
                                .send(CoreEvent::Error(e.to_string()))
                                .await;
                        }
                    }

                    // Maybe schedule conversation re-entry
                    proactive::maybe_schedule_reentry(
                        knobs, observer, pulse_bus, llm, memory, session_key, persona_soul,
                    );
                }.instrument(request_span));
            }
        });

        // Spawn autonomous task consumer
        {
            let manager = manager_for_auto;
            let observer = observer.clone();
            let pulse_bus = pulse_bus.clone();
            tokio::spawn(async move {
                let confirmer = AutoConfirmer;
                while let Some(task) = auto_rx.recv().await {
                    let manager = manager.clone();
                    let observer = observer.clone();
                    let pulse_bus = pulse_bus.clone();
                    let ghost_label = task.ghost.clone().unwrap_or_else(|| "auto".into());

                    observer.log(
                        ObserverCategory::AutonomousTask,
                        format!("Dispatching: {} → {}", ghost_label, truncate_obs(&task.goal, 80)),
                    );

                    // Spawn each task independently so they don't block the queue
                    tokio::spawn(async move {
                        match manager
                            .execute_task(&task.goal, &task.context, task.ghost.as_deref(), &confirmer)
                            .await
                        {
                            Ok(result) => {
                                observer.log(
                                    ObserverCategory::AutonomousTask,
                                    format!("Completed: {} ({} chars)", ghost_label, result.len()),
                                );
                                let pulse = Pulse::new(
                                    crate::pulse::PulseSource::AutonomousTask,
                                    crate::pulse::Urgency::Medium,
                                    result,
                                )
                                .with_target(task.target)
                                .with_ghost(ghost_label);
                                pulse_bus.send(pulse);
                            }
                            Err(e) => {
                                observer.log(
                                    ObserverCategory::AutonomousTask,
                                    format!("Failed: {} — {}", ghost_label, e),
                                );
                                tracing::error!(ghost = %ghost_label, error = %e, "Autonomous task failed");
                            }
                        }
                    });
                }
            });
        }

        Ok(CoreHandle {
            tx,
            ghosts: Arc::new(ghosts),
            memory,
            knobs,
            observer,
            pulse_bus,
            activity,
            mood,
            cron_engine: Some(cron_engine),
            delivered_rx: Arc::new(tokio::sync::Mutex::new(delivered_rx)),
            auto_tx,
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

fn truncate_obs(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.replace('\n', " ")
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", s[..end].replace('\n', " "))
    }
}
