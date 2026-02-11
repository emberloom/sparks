use crate::confirm::{Confirmer, SensitivePatterns};
use crate::config::{GhostConfig, DockerConfig};
use crate::docker::DockerSession;
use crate::error::Result;
use crate::llm::LlmProvider;
use crate::strategy::{self, TaskContract};
use crate::tools::ToolRegistry;

pub struct Executor {
    docker_config: DockerConfig,
    max_steps: usize,
    sensitive_patterns: SensitivePatterns,
}

impl Executor {
    pub fn new(docker_config: DockerConfig, max_steps: usize, sensitive_patterns: Vec<String>) -> Self {
        let compiled = SensitivePatterns::new(&sensitive_patterns);
        Self { docker_config, max_steps, sensitive_patterns: compiled }
    }

    /// Run a task contract using the specified ghost
    pub async fn run(
        &self,
        contract: &TaskContract,
        ghost: &GhostConfig,
        llm: &dyn LlmProvider,
        confirmer: &dyn Confirmer,
    ) -> Result<String> {
        tracing::info!(ghost = %ghost.name, goal = %contract.goal, "Starting executor");

        // Create session-scoped container
        let session = DockerSession::new(ghost, &self.docker_config).await?;
        let tools = ToolRegistry::for_ghost(ghost);
        let strategy = strategy::strategy_from_config(&ghost.strategy)?;

        // Run the strategy loop
        let result = strategy.run(
            contract,
            &tools,
            &session,
            llm,
            self.max_steps,
            &self.sensitive_patterns,
            confirmer,
        ).await;

        // Always clean up the container
        if let Err(e) = session.close().await {
            tracing::warn!("Failed to close container: {}", e);
        }

        match result {
            Ok(output) => {
                tracing::info!(ghost = %ghost.name, "Task completed");
                Ok(output)
            }
            Err(e) => {
                tracing::error!(ghost = %ghost.name, error = %e, "Task failed");
                Err(e)
            }
        }
    }
}
