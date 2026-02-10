pub mod react;

use async_trait::async_trait;

use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::llm::OllamaClient;
use crate::tools::ToolRegistry;

/// A task contract passed from Manager to Executor
#[derive(Debug, Clone)]
pub struct TaskContract {
    pub context: String,
    pub goal: String,
    pub constraints: Vec<String>,
}

/// Pluggable execution loop strategy
#[async_trait]
pub trait LoopStrategy: Send + Sync {
    async fn run(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &OllamaClient,
        max_steps: usize,
        sensitive_patterns: &[String],
    ) -> Result<String>;
}

/// Factory: create a strategy from config name
pub fn strategy_from_config(name: &str) -> Result<Box<dyn LoopStrategy>> {
    match name {
        "react" => Ok(Box::new(react::ReactStrategy)),
        other => Err(AthenaError::Config(format!("Unknown strategy: {}", other))),
    }
}
