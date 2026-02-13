pub mod code;
pub mod react;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::confirm::Confirmer;
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::llm::LlmProvider;
use crate::tools::ToolRegistry;

/// Channel for sending core events (status, stream chunks) to the frontend.
pub type StatusSender = mpsc::Sender<CoreEvent>;

/// A task contract passed from Manager to Executor
#[derive(Debug, Clone)]
pub struct TaskContract {
    pub context: String,
    pub goal: String,
    pub constraints: Vec<String>,
    /// Ghost soul — identity document prepended to the system prompt
    pub soul: Option<String>,
    /// Tool reference document — detailed usage guide injected into system prompt
    pub tools_doc: Option<String>,
    /// Preferred CLI tool for code strategy (from runtime knob)
    pub cli_tool_preference: Option<String>,
}

/// Pluggable execution loop strategy
#[async_trait]
pub trait LoopStrategy: Send + Sync {
    async fn run(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        max_steps: usize,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<String>;
}

/// Factory: create a strategy from config name
pub fn strategy_from_config(name: &str) -> Result<Box<dyn LoopStrategy>> {
    match name {
        "react" => Ok(Box::new(react::ReactStrategy)),
        "code" => Ok(Box::new(code::CodeStrategy)),
        other => Err(AthenaError::Config(format!("Unknown strategy: {}", other))),
    }
}
