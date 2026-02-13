use std::path::PathBuf;

use serde_json::Value;

use crate::confirm::{Confirmer, SensitivePatterns};
use crate::config::{GhostConfig, DockerConfig};
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::llm::LlmProvider;
use crate::self_heal;
use crate::strategy::{self, StatusSender, TaskContract};
use crate::tools::ToolRegistry;

pub struct Executor {
    docker_config: DockerConfig,
    max_steps: usize,
    sensitive_patterns: SensitivePatterns,
    dynamic_tools_path: Option<PathBuf>,
}

impl Executor {
    pub fn new(
        docker_config: DockerConfig,
        max_steps: usize,
        sensitive_patterns: Vec<String>,
        dynamic_tools_path: Option<PathBuf>,
    ) -> Self {
        let compiled = SensitivePatterns::new(&sensitive_patterns);
        Self { docker_config, max_steps, sensitive_patterns: compiled, dynamic_tools_path }
    }

    /// Run a task contract using the specified ghost
    #[tracing::instrument(skip(self, contract, llm, confirmer, status_tx), fields(ghost = %ghost.name))]
    pub async fn run(
        &self,
        contract: &TaskContract,
        ghost: &GhostConfig,
        llm: &dyn LlmProvider,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<String> {
        tracing::info!(ghost = %ghost.name, goal = %contract.goal, "Starting executor");

        // Create session-scoped container
        let session = DockerSession::new(ghost, &self.docker_config).await?;
        let tools = ToolRegistry::for_ghost(ghost, self.dynamic_tools_path.as_deref());
        let strategy = strategy::strategy_from_config(&ghost.strategy)?;

        // Run the strategy loop
        let result = strategy.run(
            contract,
            &tools,
            &session,
            llm,
            self.max_steps,
            self,
            confirmer,
            status_tx,
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

    /// Execute a tool with confirmation handling and self-heal hints.
    /// Centralizes tool execution logic so strategies don't call `tool.execute()` directly.
    #[tracing::instrument(skip(self, json, tools, docker, confirmer), fields(tool = tool_name))]
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        json: &Value,
        tools: &ToolRegistry,
        docker: &DockerSession,
        confirmer: &dyn Confirmer,
    ) -> Result<String> {
        let params = json.get("params").cloned().unwrap_or_default();

        let tool = tools
            .get(tool_name)
            .ok_or_else(|| AthenaError::Tool(format!("Unknown tool: {}", tool_name)))?;

        // Confirmation check
        let needs_confirm = if tool.needs_confirmation() {
            true
        } else if tool_name == "shell" {
            params
                .get("command")
                .and_then(|v| v.as_str())
                .map(|cmd| self.sensitive_patterns.is_match(cmd))
                .unwrap_or(false)
        } else {
            false
        };

        if needs_confirm {
            let action_desc = format!(
                "[{}] {}",
                tool_name,
                params
                    .get("command")
                    .or_else(|| params.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(action)")
            );

            match confirmer.confirm(&action_desc).await {
                Ok(true) => {} // approved
                _ => {
                    return Ok(
                        "The user denied this action. Try a different approach.".to_string()
                    );
                }
            }
        }

        let result = tool.execute(docker, &params).await;

        match result {
            Ok(r) => {
                let base = if r.success {
                    format!("[tool result]\n{}", r.output)
                } else {
                    format!("[tool error]\n{}", r.output)
                };
                if !r.success {
                    let synth_err = AthenaError::Tool(r.output.clone());
                    if let Some(fix) = self_heal::attempt_fix(&synth_err, tool_name, &params) {
                        Ok(format!(
                            "{}\n\n[self-heal hint]\nGoal: {}\nContext: {}\nConstraints: {}",
                            base,
                            fix.goal,
                            fix.context,
                            fix.constraints.join("; "),
                        ))
                    } else {
                        Ok(base)
                    }
                } else {
                    Ok(base)
                }
            }
            Err(ref e) => {
                let base = format!("[tool error]\n{}", e);
                if let Some(fix) = self_heal::attempt_fix(e, tool_name, &params) {
                    Ok(format!(
                        "{}\n\n[self-heal hint]\nGoal: {}\nContext: {}\nConstraints: {}",
                        base,
                        fix.goal,
                        fix.context,
                        fix.constraints.join("; "),
                    ))
                } else {
                    Ok(base)
                }
            }
        }
    }
}
