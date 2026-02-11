use crate::confirm::Confirmer;
use crate::config::{AgentConfig, DockerConfig};
use crate::docker::DockerSession;
use crate::error::Result;
use crate::llm::LlmProvider;
use crate::strategy::{self, TaskContract};
use crate::tools::ToolRegistry;

pub struct Executor {
    docker_config: DockerConfig,
    max_steps: usize,
    sensitive_patterns: Vec<String>,
}

impl Executor {
    pub fn new(docker_config: DockerConfig, max_steps: usize, sensitive_patterns: Vec<String>) -> Self {
        Self { docker_config, max_steps, sensitive_patterns }
    }

    /// Run a task contract using the specified agent
    pub async fn run(
        &self,
        contract: &TaskContract,
        agent: &AgentConfig,
        llm: &dyn LlmProvider,
        confirmer: &dyn Confirmer,
    ) -> Result<String> {
        tracing::info!(agent = %agent.name, goal = %contract.goal, "Starting executor");

        // Create session-scoped container
        let session = DockerSession::new(agent, &self.docker_config).await?;
        let tools = ToolRegistry::for_agent(agent);
        let strategy = strategy::strategy_from_config(&agent.strategy)?;

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
                tracing::info!(agent = %agent.name, "Task completed");
                Ok(output)
            }
            Err(e) => {
                tracing::error!(agent = %agent.name, error = %e, "Task failed");
                Err(e)
            }
        }
    }
}
