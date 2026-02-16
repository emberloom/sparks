use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::config::{Config, GhostConfig};
use crate::confirm::{AutoConfirmer, Confirmer};
use crate::embeddings::Embedder;
use crate::error::Result;
use crate::heartbeat;
use crate::introspect::{self, SharedMetrics, SystemMetrics};
use crate::kpi::TaskOutcomeStore;
use crate::knobs::{RuntimeKnobs, SharedKnobs};
use crate::langfuse::SharedLangfuse;
use crate::llm::LlmProvider;
use crate::manager::Manager;
use crate::memory::MemoryStore;
use crate::mood::MoodState;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::proactive::{self, ActivityTracker};
use crate::profiles;
use crate::pulse::{self, Pulse, PulseBus};
use crate::randomness;
use crate::scheduler::CronEngine;
use crate::tool_usage::ToolUsageStore;

const STALE_STARTED_TASK_SECS: u64 = 30 * 60;
const STALE_STARTED_REASON: &str = "stale_started_timeout";

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
    /// Incremental text chunk from the LLM (streamed)
    StreamChunk(String),
    /// A tool has finished executing
    ToolRun {
        tool: String,
        result: String,
        success: bool,
    },
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
    /// Mission lane for KPI attribution.
    pub lane: String,
    /// Risk tier for KPI attribution.
    pub risk_tier: String,
    /// Repo/product label for KPI attribution.
    pub repo: String,
    /// Optional caller-supplied task id for correlation.
    pub task_id: Option<String>,
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
    pub llm: Arc<dyn LlmProvider>,
    pub knobs: SharedKnobs,
    pub observer: ObserverHandle,
    pub pulse_bus: PulseBus,
    pub activity: Arc<ActivityTracker>,
    pub mood: Arc<MoodState>,
    pub cron_engine: Option<Arc<CronEngine>>,
    pub delivered_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Pulse>>>,
    pub auto_tx: mpsc::Sender<AutonomousTask>,
    pub metrics: SharedMetrics,
}

impl CoreHandle {
    /// Submit an autonomous task for background execution by a ghost.
    /// Results are delivered as pulses to the specified target.
    pub async fn dispatch_task(&self, mut task: AutonomousTask) -> Result<String> {
        let task_id = task
            .task_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        task.task_id = Some(task_id.clone());
        self.auto_tx.send(task).await.map_err(|_| {
            crate::error::AthenaError::Tool("Autonomous task queue full or shut down".into())
        })?;
        Ok(task_id)
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
        self.tx
            .send(req)
            .await
            .map_err(|_| crate::error::AthenaError::Tool("Core task has shut down".into()))?;
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

struct CoreRuntimeHandles {
    observer: ObserverHandle,
    knobs: SharedKnobs,
    langfuse: SharedLangfuse,
    mood: Arc<MoodState>,
    pulse_bus: PulseBus,
    delivered_rx: mpsc::Receiver<Pulse>,
    activity: Arc<ActivityTracker>,
    auto_tx: mpsc::Sender<AutonomousTask>,
    auto_rx: mpsc::Receiver<AutonomousTask>,
    usage_store: Arc<ToolUsageStore>,
    outcome_store: Arc<TaskOutcomeStore>,
    cron_engine: Arc<CronEngine>,
    metrics: SharedMetrics,
}

impl AthenaCore {
    pub async fn start(config: Config, memory: Arc<MemoryStore>) -> Result<CoreHandle> {
        let (llm, selected_provider) = connect_main_llm(&config).await?;
        let llm_for_handle = llm.clone();
        let orchestrator = connect_orchestrator(&config, &selected_provider, &llm).await?;
        let embedder = init_embedder_opt(&config).await;

        spawn_embedding_backfill(memory.clone(), embedder.clone());

        let (merged_ghosts, ghosts) = load_ghost_profiles(&config)?;
        let CoreRuntimeHandles {
            observer,
            knobs,
            langfuse,
            mood,
            pulse_bus,
            delivered_rx,
            activity,
            auto_tx,
            auto_rx,
            usage_store,
            outcome_store,
            cron_engine,
            metrics,
        } = init_runtime_handles(&config, memory.clone(), llm.clone())?;
        let manager = build_manager(
            &config,
            merged_ghosts,
            llm,
            orchestrator,
            memory.clone(),
            embedder,
            mood.clone(),
            knobs.clone(),
            usage_store,
            metrics.clone(),
            langfuse.clone(),
            observer.clone(),
        );
        let persona_soul_for_reentry = config.persona.soul.clone();

        let (tx, rx) = mpsc::channel::<CoreRequest>(32);

        let llm_for_reentry = manager.llm_ref();
        spawn_core_event_loop(
            rx,
            manager.clone(),
            activity.clone(),
            knobs.clone(),
            observer.clone(),
            pulse_bus.clone(),
            memory.clone(),
            llm_for_reentry,
            persona_soul_for_reentry,
            langfuse.clone(),
        );
        spawn_autonomous_task_consumer(
            auto_rx,
            manager,
            observer.clone(),
            pulse_bus.clone(),
            memory.clone(),
            outcome_store,
        );

        Ok(CoreHandle {
            tx,
            ghosts: Arc::new(ghosts),
            memory,
            llm: llm_for_handle,
            knobs,
            observer,
            pulse_bus,
            activity,
            mood,
            cron_engine: Some(cron_engine),
            delivered_rx: Arc::new(tokio::sync::Mutex::new(delivered_rx)),
            auto_tx,
            metrics,
        })
    }
}

fn init_runtime_handles(
    config: &Config,
    memory: Arc<MemoryStore>,
    llm: Arc<dyn LlmProvider>,
) -> Result<CoreRuntimeHandles> {
    let observer = init_observer();
    let knobs: SharedKnobs = Arc::new(std::sync::RwLock::new(RuntimeKnobs::from_config(config)));
    let langfuse = init_langfuse_client(config, &knobs);
    if langfuse.is_some() {
        tracing::info!("Langfuse observability enabled");
    }
    let mood = Arc::new(MoodState::load(&memory, config.mood.timezone_offset));
    let (pulse_bus, delivered_rx) = init_pulse_bus(&observer, &knobs);
    let activity = Arc::new(ActivityTracker::new());
    let (auto_tx, auto_rx) = mpsc::channel::<AutonomousTask>(32);
    let usage_store = create_usage_store(config)?;
    let outcome_store = create_task_outcome_store(config)?;
    expire_stale_started_tasks(&outcome_store, &observer);

    spawn_housekeeping_loops(
        config,
        memory.clone(),
        llm.clone(),
        knobs.clone(),
        observer.clone(),
        pulse_bus.clone(),
        mood.clone(),
        activity.clone(),
        auto_tx.clone(),
        langfuse.clone(),
    );
    let cron_engine = init_cron_engine(
        memory.clone(),
        observer.clone(),
        pulse_bus.clone(),
        llm,
        knobs.clone(),
        langfuse.clone(),
    );
    let metrics = init_metrics_collector(
        config,
        knobs.clone(),
        observer.clone(),
        memory,
        usage_store.clone(),
        auto_tx.clone(),
        langfuse.clone(),
    );

    Ok(CoreRuntimeHandles {
        observer,
        knobs,
        langfuse,
        mood,
        pulse_bus,
        delivered_rx,
        activity,
        auto_tx,
        auto_rx,
        usage_store,
        outcome_store,
        cron_engine,
        metrics,
    })
}

fn spawn_embedding_backfill(memory: Arc<MemoryStore>, embedder: Option<Arc<Embedder>>) {
    if let Some(embedder) = embedder {
        tokio::task::spawn_blocking(move || {
            backfill_embeddings(&memory, &embedder);
        });
    }
}

fn init_observer() -> ObserverHandle {
    let observer = ObserverHandle::new(1024);
    crate::observer::spawn_uds_listener(observer.clone());
    observer.log(
        ObserverCategory::Startup,
        "Athena core started, observer active",
    );
    observer
}

fn init_pulse_bus(
    observer: &ObserverHandle,
    knobs: &SharedKnobs,
) -> (PulseBus, mpsc::Receiver<Pulse>) {
    let pulse_bus = PulseBus::new(256);
    let (delivered_tx, delivered_rx) = mpsc::channel::<Pulse>(64);
    pulse::spawn_pulse_consumer(
        pulse_bus.clone(),
        observer.clone(),
        delivered_tx,
        knobs.clone(),
    );
    (pulse_bus, delivered_rx)
}

#[allow(clippy::too_many_arguments)]
fn spawn_housekeeping_loops(
    config: &Config,
    memory: Arc<MemoryStore>,
    llm: Arc<dyn LlmProvider>,
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    mood: Arc<MoodState>,
    activity: Arc<ActivityTracker>,
    auto_tx: mpsc::Sender<AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    spawn_conversation_cleanup(memory.clone());
    spawn_mood_drift_loop(
        mood.clone(),
        knobs.clone(),
        observer.clone(),
        memory.clone(),
    );

    heartbeat::spawn_heartbeat_loop(
        knobs.clone(),
        observer.clone(),
        pulse_bus.clone(),
        llm.clone(),
        memory.clone(),
        mood,
        config.heartbeat.soul_file.clone(),
        langfuse.clone(),
    );
    proactive::spawn_memory_scanner(
        knobs.clone(),
        observer.clone(),
        pulse_bus.clone(),
        llm.clone(),
        memory.clone(),
        auto_tx.clone(),
        langfuse.clone(),
    );
    proactive::spawn_idle_musings(
        knobs.clone(),
        observer.clone(),
        pulse_bus.clone(),
        llm.clone(),
        memory.clone(),
        activity,
        auto_tx.clone(),
        langfuse.clone(),
    );
    proactive::spawn_code_indexer(
        knobs.clone(),
        observer.clone(),
        auto_tx.clone(),
        langfuse.clone(),
    );
    proactive::spawn_refactoring_scanner(
        knobs.clone(),
        observer.clone(),
        llm,
        memory.clone(),
        auto_tx.clone(),
        langfuse.clone(),
    );
}

fn init_cron_engine(
    memory: Arc<MemoryStore>,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    knobs: SharedKnobs,
    langfuse: SharedLangfuse,
) -> Arc<CronEngine> {
    let cron_engine = Arc::new(CronEngine::new(
        memory, observer, pulse_bus, llm, knobs, langfuse,
    ));
    cron_engine.clone().spawn_tick_loop();
    cron_engine
}

fn init_metrics_collector(
    config: &Config,
    knobs: SharedKnobs,
    observer: ObserverHandle,
    memory: Arc<MemoryStore>,
    usage_store: Arc<ToolUsageStore>,
    auto_tx: mpsc::Sender<AutonomousTask>,
    langfuse: SharedLangfuse,
) -> SharedMetrics {
    let metrics: SharedMetrics = Arc::new(std::sync::RwLock::new(SystemMetrics::default()));
    let db_path = config.db_path().unwrap_or_default();
    introspect::spawn_metrics_collector(
        knobs,
        observer,
        metrics.clone(),
        memory,
        usage_store,
        db_path,
        auto_tx,
        langfuse,
    );
    metrics
}

#[allow(clippy::too_many_arguments)]
fn build_manager(
    config: &Config,
    merged_ghosts: Vec<GhostConfig>,
    llm: Arc<dyn LlmProvider>,
    orchestrator: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    embedder: Option<Arc<Embedder>>,
    mood: Arc<MoodState>,
    knobs: SharedKnobs,
    usage_store: Arc<ToolUsageStore>,
    metrics: SharedMetrics,
    langfuse: SharedLangfuse,
    observer: ObserverHandle,
) -> Arc<Manager> {
    let manager = Arc::new(Manager::new(
        config,
        merged_ghosts,
        llm,
        orchestrator,
        memory,
        embedder,
        config.persona.soul.clone(),
        config.persona.self_knowledge.clone(),
        config.persona.tools_doc.clone(),
        mood,
        knobs,
        usage_store,
        metrics,
        langfuse,
    ));
    if let Some(dt_path) = manager.dynamic_tools_path() {
        crate::dynamic_tools::spawn_hot_reload(
            dt_path.clone(),
            manager.host_workspace().to_string(),
            manager.direct_tools_ref(),
            observer,
        );
    }
    manager
}

#[allow(clippy::too_many_arguments)]
fn spawn_core_event_loop(
    mut rx: mpsc::Receiver<CoreRequest>,
    manager: Arc<Manager>,
    activity: Arc<ActivityTracker>,
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    memory: Arc<MemoryStore>,
    llm: Arc<dyn LlmProvider>,
    persona_soul: Option<String>,
    langfuse: SharedLangfuse,
) {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            tracing::debug!(input = %req.input, "Core received request");
            let request_span = tracing::info_span!("request", id = %uuid::Uuid::new_v4());
            tokio::spawn(
                handle_core_request(
                    req,
                    manager.clone(),
                    activity.clone(),
                    knobs.clone(),
                    observer.clone(),
                    pulse_bus.clone(),
                    memory.clone(),
                    llm.clone(),
                    persona_soul.clone(),
                    langfuse.clone(),
                )
                .instrument(request_span),
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn handle_core_request(
    req: CoreRequest,
    manager: Arc<Manager>,
    activity: Arc<ActivityTracker>,
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    memory: Arc<MemoryStore>,
    llm: Arc<dyn LlmProvider>,
    persona_soul: Option<String>,
    langfuse: SharedLangfuse,
) {
    activity.touch();
    let session_key = req.session.session_key();
    observer.emit(crate::observer::ObserverEvent::new(
        ObserverCategory::ChatIn,
        format!("{} \"{}\"", session_key, truncate_obs(&req.input, 80)),
    ));
    let _ = req
        .event_tx
        .send(CoreEvent::Status("Thinking...".into()))
        .await;

    let (status_tx, status_rx) = mpsc::channel::<CoreEvent>(16);
    let bridge_handle = spawn_status_bridge(status_rx, req.event_tx.clone());

    tracing::debug!("Calling manager.handle()");
    let result = manager
        .handle(
            &req.input,
            &req.session,
            req.confirmer.as_ref(),
            Some(&status_tx),
        )
        .await;

    drop(status_tx);
    let _ = bridge_handle.await;

    match result {
        Ok(response) => {
            tracing::debug!(len = response.len(), "Manager returned response");
            observer.emit(
                crate::observer::ObserverEvent::new(
                    ObserverCategory::ChatOut,
                    format!("{} ({} chars)", session_key, response.len()),
                )
                .with_details(truncate_obs(&response, 100)),
            );
            let _ = req.event_tx.send(CoreEvent::Response(response)).await;
        }
        Err(e) => {
            tracing::error!(error = %e, "Manager returned error");
            observer.log(
                ObserverCategory::ChatOut,
                format!("{} ERROR: {}", session_key, e),
            );
            let _ = req.event_tx.send(CoreEvent::Error(e.to_string())).await;
        }
    }

    proactive::maybe_schedule_reentry(
        knobs,
        observer,
        pulse_bus,
        llm,
        memory,
        session_key,
        persona_soul,
        langfuse,
    );
}

fn spawn_status_bridge(
    mut status_rx: mpsc::Receiver<CoreEvent>,
    event_tx: mpsc::Sender<CoreEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = status_rx.recv().await {
            let _ = event_tx.send(event).await;
        }
    })
}

fn spawn_autonomous_task_consumer(
    mut auto_rx: mpsc::Receiver<AutonomousTask>,
    manager: Arc<Manager>,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    memory: Arc<MemoryStore>,
    outcome_store: Arc<TaskOutcomeStore>,
) {
    tokio::spawn(async move {
        while let Some(task) = auto_rx.recv().await {
            let manager = manager.clone();
            let observer = observer.clone();
            let pulse_bus = pulse_bus.clone();
            let memory = memory.clone();
            let outcome_store = outcome_store.clone();
            tokio::spawn(async move {
                execute_autonomous_task(task, manager, observer, pulse_bus, memory, outcome_store)
                    .await;
            });
        }
    });
}

async fn execute_autonomous_task(
    task: AutonomousTask,
    manager: Arc<Manager>,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    memory: Arc<MemoryStore>,
    outcome_store: Arc<TaskOutcomeStore>,
) {
    let confirmer = AutoConfirmer;
    expire_stale_started_tasks(&outcome_store, &observer);
    let task_id = task
        .task_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let ghost_label = task.ghost.clone().unwrap_or_else(|| "auto".into());
    let goal_summary = truncate_obs(&task.goal, 120);
    record_autonomous_task_start(&outcome_store, &task_id, &task);
    log_autonomous_dispatch(&observer, &task, &ghost_label);
    introspect::inc_active_tasks();

    let (verification_total_on_fail, _) = infer_verification_counters(&task.goal, None, false);
    let rolled_back_on_fail = infer_rollback_flag(&task.goal, None);
    match manager
        .execute_task(&task.goal, &task.context, task.ghost.as_deref(), &confirmer)
        .await
    {
        Ok(result) => handle_autonomous_task_success(
            &task,
            &task_id,
            &ghost_label,
            &goal_summary,
            result,
            &observer,
            &pulse_bus,
            &memory,
            &outcome_store,
        ),
        Err(e) => handle_autonomous_task_failure(
            &task,
            &task_id,
            &ghost_label,
            &goal_summary,
            &e,
            verification_total_on_fail,
            rolled_back_on_fail,
            &observer,
            &pulse_bus,
            &memory,
            &outcome_store,
        ),
    }
    introspect::dec_active_tasks();
}

fn record_autonomous_task_start(
    outcome_store: &TaskOutcomeStore,
    task_id: &str,
    task: &AutonomousTask,
) {
    let _ = outcome_store.record_start(
        task_id,
        &task.lane,
        &task.repo,
        &task.risk_tier,
        task.ghost.as_deref(),
        &task.goal,
    );
}

fn log_autonomous_dispatch(observer: &ObserverHandle, task: &AutonomousTask, ghost_label: &str) {
    observer.log(
        ObserverCategory::AutonomousTask,
        format!(
            "Dispatching [{}:{}:{}]: {} → {}",
            task.lane,
            task.repo,
            task.risk_tier,
            ghost_label,
            truncate_obs(&task.goal, 80)
        ),
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_autonomous_task_success(
    task: &AutonomousTask,
    task_id: &str,
    ghost_label: &str,
    goal_summary: &str,
    result: String,
    observer: &ObserverHandle,
    pulse_bus: &PulseBus,
    memory: &MemoryStore,
    outcome_store: &TaskOutcomeStore,
) {
    let (verification_total, verification_passed) =
        infer_verification_counters(&task.goal, Some(&result), true);
    let rolled_back = infer_rollback_flag(&task.goal, Some(&result));
    let _ = outcome_store.record_finish(
        task_id,
        "succeeded",
        verification_total,
        verification_passed,
        rolled_back,
        None,
    );
    observer.log(
        ObserverCategory::AutonomousTask,
        format!("Completed: {} ({} chars)", ghost_label, result.len()),
    );
    auto_store_task_result(task, &result, observer, memory);

    let outcome = format!(
        "Autonomous task succeeded [{}]: {}\nResult summary: {}",
        ghost_label,
        goal_summary,
        truncate_obs(&result, 200),
    );
    let _ = memory.store("code_change", &outcome, None);

    let pulse = Pulse::new(
        crate::pulse::PulseSource::AutonomousTask,
        crate::pulse::Urgency::Medium,
        result,
    )
    .with_task_id(task_id.to_string())
    .with_target(task.target.clone())
    .with_ghost(ghost_label.to_string());
    pulse_bus.send(pulse);
}

#[allow(clippy::too_many_arguments)]
fn handle_autonomous_task_failure(
    task: &AutonomousTask,
    task_id: &str,
    ghost_label: &str,
    goal_summary: &str,
    err: &crate::error::AthenaError,
    verification_total: u64,
    rolled_back: bool,
    observer: &ObserverHandle,
    pulse_bus: &PulseBus,
    memory: &MemoryStore,
    outcome_store: &TaskOutcomeStore,
) {
    let _ = outcome_store.record_finish(
        task_id,
        "failed",
        verification_total,
        0,
        rolled_back,
        Some(&err.to_string()),
    );
    observer.log(
        ObserverCategory::AutonomousTask,
        format!("Failed: {} — {}", ghost_label, err),
    );
    tracing::error!(ghost = %ghost_label, error = %err, "Autonomous task failed");

    let outcome = format!(
        "Autonomous task FAILED [{}]: {}\nError: {}",
        ghost_label, goal_summary, err,
    );
    let _ = memory.store(failure_category(goal_summary), &outcome, None);

    let pulse = Pulse::new(
        crate::pulse::PulseSource::AutonomousTask,
        crate::pulse::Urgency::High,
        format!("Task failed [{}]: {}", ghost_label, err),
    )
    .with_task_id(task_id.to_string())
    .with_target(task.target.clone())
    .with_ghost(ghost_label.to_string());
    // Emit a failure pulse so synchronous waiters can complete deterministically.
    observer.log(
        ObserverCategory::AutonomousTask,
        format!("Failure pulse emitted for task_id={}", task_id),
    );
    pulse_bus.send(pulse);
}

fn infer_verification_counters(goal: &str, result: Option<&str>, success: bool) -> (u64, u64) {
    let goal_lower = goal.to_lowercase();
    let result_lower = result.unwrap_or_default().to_lowercase();
    let has_verify = [
        "test",
        "lint",
        "verify",
        "cargo check",
        "cargo test",
        "pytest",
        "npm test",
        "go test",
    ]
    .iter()
    .any(|k| goal_lower.contains(k) || result_lower.contains(k));
    if !has_verify {
        return (0, 0);
    }
    if success { (1, 1) } else { (1, 0) }
}

fn infer_rollback_flag(goal: &str, result: Option<&str>) -> bool {
    let goal_lower = goal.to_lowercase();
    let result_lower = result.unwrap_or_default().to_lowercase();
    ["rollback", "roll back", "revert"]
        .iter()
        .any(|k| goal_lower.contains(k) || result_lower.contains(k))
}

fn auto_store_task_result(
    task: &AutonomousTask,
    result: &str,
    observer: &ObserverHandle,
    memory: &MemoryStore,
) {
    if let Some(start) = task.context.find("[auto_store:") {
        let after = &task.context[start + 12..];
        if let Some(end) = after.find(']') {
            let category = &after[..end];
            let _ = memory.store(category, result, None);
            observer.log(
                ObserverCategory::AutonomousTask,
                format!("Auto-stored result as '{}'", category),
            );
        }
    }
}

fn failure_category(goal_summary: &str) -> &'static str {
    let goal_lower = goal_summary.to_lowercase();
    if goal_lower.contains("refactor") {
        "refactoring_failed"
    } else if goal_lower.contains("improvement idea") {
        "improvement_idea_failed"
    } else {
        "code_change_failed"
    }
}

async fn connect_main_llm(config: &Config) -> Result<(Arc<dyn LlmProvider>, String)> {
    let mut llm: Option<Arc<dyn LlmProvider>> = None;
    let mut selected_provider = config.llm.provider.clone();
    let mut last_err: Option<crate::error::AthenaError> = None;

    for provider_name in config.provider_candidates() {
        let candidate = match config.build_llm_provider_for(&provider_name) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    provider = %provider_name,
                    error = %e,
                    "Skipping LLM provider candidate"
                );
                last_err = Some(e);
                continue;
            }
        };

        eprint!("Connecting to {}... ", candidate.provider_name());
        match candidate.health_check().await {
            Ok(()) => {
                eprintln!("ok");
                selected_provider = provider_name;
                llm = Some(candidate);
                break;
            }
            Err(e) => {
                eprintln!("failed: {}", e);
                tracing::warn!(
                    provider = %provider_name,
                    error = %e,
                    "LLM provider candidate unreachable"
                );
                last_err = Some(e);
            }
        }
    }

    let llm = llm.ok_or_else(|| {
        last_err.unwrap_or_else(|| {
            crate::error::AthenaError::Config("No reachable LLM provider candidates".into())
        })
    })?;

    Ok((llm, selected_provider))
}

async fn connect_orchestrator(
    config: &Config,
    selected_provider: &str,
    llm: &Arc<dyn LlmProvider>,
) -> Result<Arc<dyn LlmProvider>> {
    let orchestrator = config.build_orchestrator_provider_for(selected_provider, llm)?;
    if orchestrator.provider_name() == llm.provider_name() {
        return Ok(orchestrator);
    }

    eprint!("Connecting to {}... ", orchestrator.provider_name());
    match orchestrator.health_check().await {
        Ok(()) => {
            eprintln!("ok");
            Ok(orchestrator)
        }
        Err(e) => {
            eprintln!("failed: {}", e);
            tracing::warn!(
                error = %e,
                "Orchestrator provider unreachable, falling back to main provider"
            );
            Ok(llm.clone())
        }
    }
}

async fn init_embedder_opt(config: &Config) -> Option<Arc<Embedder>> {
    if !config.embedding.enabled {
        tracing::info!("Embedding model disabled in config");
        return None;
    }

    let cfg = config.clone();
    match tokio::task::spawn_blocking(move || init_embedder(&cfg)).await {
        Ok(Ok(e)) => Some(Arc::new(e)),
        Ok(Err(e)) => {
            tracing::warn!(
                "Embedding model unavailable, falling back to keyword search: {}",
                e
            );
            None
        }
        Err(e) => {
            tracing::warn!("Embedder init task panicked: {}", e);
            None
        }
    }
}

fn load_ghost_profiles(config: &Config) -> Result<(Vec<GhostConfig>, Vec<GhostInfo>)> {
    let merged_ghosts = profiles::load_ghosts(config)?;
    let ghosts = merged_ghosts
        .iter()
        .map(|g| GhostInfo {
            name: g.name.clone(),
            description: g.description.clone(),
            tools: g.tools.clone(),
            strategy: g.strategy.clone(),
        })
        .collect();
    Ok((merged_ghosts, ghosts))
}

fn init_langfuse_client(config: &Config, knobs: &SharedKnobs) -> SharedLangfuse {
    let k = knobs.read().unwrap();
    if !k.langfuse_enabled {
        return None;
    }

    let cfg = &config.langfuse;
    let public_key = cfg
        .public_key
        .clone()
        .or_else(|| std::env::var("LANGFUSE_PUBLIC_KEY").ok());
    let secret_key = cfg
        .secret_key
        .clone()
        .or_else(|| std::env::var("LANGFUSE_SECRET_KEY").ok());
    let base_url = cfg
        .base_url
        .clone()
        .or_else(|| std::env::var("LANGFUSE_BASE_URL").ok());

    match (public_key, secret_key) {
        (Some(pk), Some(sk)) => Some(Arc::new(crate::langfuse::LangfuseClient::new(
            pk, sk, base_url,
        ))),
        _ => {
            tracing::warn!("Langfuse enabled but credentials missing");
            None
        }
    }
}

fn spawn_conversation_cleanup(memory: Arc<MemoryStore>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Ok(n) = memory.cleanup_conversations(7) {
                if n > 0 {
                    tracing::info!("Cleaned up {} old conversation turns", n);
                }
            }
        }
    });
}

fn spawn_mood_drift_loop(
    mood: Arc<MoodState>,
    knobs: SharedKnobs,
    observer: ObserverHandle,
    memory: Arc<MemoryStore>,
) {
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
            mood.save(&memory);
        }
    });
}

fn create_usage_store(config: &Config) -> Result<Arc<ToolUsageStore>> {
    let db_path = config.db_path().map_err(|e| {
        crate::error::AthenaError::Config(format!(
            "Failed to resolve DB path for usage store: {}",
            e
        ))
    })?;
    let conn = rusqlite::Connection::open(&db_path)?;
    let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    Ok(Arc::new(ToolUsageStore::new(conn)))
}

fn create_task_outcome_store(config: &Config) -> Result<Arc<TaskOutcomeStore>> {
    let db_path = config.db_path().map_err(|e| {
        crate::error::AthenaError::Config(format!(
            "Failed to resolve DB path for task outcome store: {}",
            e
        ))
    })?;
    let conn = rusqlite::Connection::open(&db_path)?;
    let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    Ok(Arc::new(TaskOutcomeStore::new(conn)))
}

fn expire_stale_started_tasks(outcome_store: &TaskOutcomeStore, observer: &ObserverHandle) {
    match outcome_store.fail_stale_started_tasks(STALE_STARTED_TASK_SECS, STALE_STARTED_REASON) {
        Ok(0) => {}
        Ok(n) => observer.log(
            ObserverCategory::AutonomousTask,
            format!(
                "Marked {} stale started task(s) as failed (threshold={}s)",
                n, STALE_STARTED_TASK_SECS
            ),
        ),
        Err(e) => tracing::warn!("Failed to mark stale started tasks: {}", e),
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
