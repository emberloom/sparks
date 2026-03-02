use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::config::{DockerConfig, GhostConfig};
use crate::confirm::{Confirmer, SensitivePatterns};
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::llm::LlmProvider;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::self_heal;
use crate::strategy::{self, StatusSender, TaskContract};
use crate::tool_usage::ToolUsageStore;
use crate::tools::ToolRegistry;

pub struct Executor {
    docker_config: DockerConfig,
    max_steps: usize,
    sensitive_patterns: SensitivePatterns,
    dynamic_tools_path: Option<PathBuf>,
    knobs: SharedKnobs,
    github_token: Option<String>,
    usage_store: Arc<ToolUsageStore>,
    observer: ObserverHandle,
    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    langfuse: SharedLangfuse,
}

impl Executor {
    pub fn new(
        docker_config: DockerConfig,
        max_steps: usize,
        sensitive_patterns: Vec<String>,
        dynamic_tools_path: Option<PathBuf>,
        knobs: SharedKnobs,
        github_token: Option<String>,
        usage_store: Arc<ToolUsageStore>,
        observer: ObserverHandle,
        langfuse: SharedLangfuse,
    ) -> Self {
        let compiled = SensitivePatterns::new(&sensitive_patterns);
        Self {
            docker_config,
            max_steps,
            sensitive_patterns: compiled,
            dynamic_tools_path,
            knobs,
            github_token,
            usage_store,
            observer,
            langfuse,
        }
    }

    /// Run a task contract using the specified ghost
    #[tracing::instrument(skip(self, contract, llm, confirmer, status_tx, trace), fields(ghost = %ghost.name))]
    pub async fn run(
        &self,
        contract: &TaskContract,
        ghost: &GhostConfig,
        llm: &dyn LlmProvider,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        tracing::info!(ghost = %ghost.name, goal = %contract.goal, "Starting executor");

        let run_span = trace.map(|t| t.span("ghost_run", Some(&contract.goal)));

        // Create session-scoped container
        let session = DockerSession::new(ghost, &self.docker_config).await?;
        let tools = ToolRegistry::for_ghost(
            ghost,
            self.dynamic_tools_path.as_deref(),
            self.knobs.clone(),
            self.github_token.clone(),
            Some(self.usage_store.clone()),
        );
        let strategy = strategy::strategy_from_config(&ghost.strategy)?;

        // Try direct tool completion first (precheck)
        if let Some(result) = strategy::try_direct_completion(
            contract, &tools, &session, llm, self, confirmer, status_tx, trace,
        )
        .await?
        {
            tracing::info!(ghost = %ghost.name, "Task completed via direct tool use (precheck)");
            if let Err(e) = session.close().await {
                tracing::warn!("Failed to close container: {}", e);
            }
            if let Some(s) = run_span {
                let preview = if result.len() > 500 {
                    &result[..result.floor_char_boundary(500)]
                } else {
                    &result
                };
                s.end(Some(preview));
            }
            return Ok(result);
        }

        // Run the strategy loop
        let result = strategy
            .run(
                contract,
                &tools,
                &session,
                llm,
                self.max_steps,
                self,
                confirmer,
                status_tx,
                trace,
            )
            .await;

        // Always clean up the container
        if let Err(e) = session.close().await {
            tracing::warn!("Failed to close container: {}", e);
        }

        match result {
            Ok(output) => {
                tracing::info!(ghost = %ghost.name, "Task completed");
                if let Some(s) = run_span {
                    let preview = if output.len() > 500 {
                        &output[..output.floor_char_boundary(500)]
                    } else {
                        &output
                    };
                    s.end(Some(preview));
                }
                Ok(output)
            }
            Err(e) => {
                tracing::error!(ghost = %ghost.name, error = %e, "Task failed");
                if let Some(s) = run_span {
                    s.end(Some(&format!("error: {}", e)));
                }
                Err(e)
            }
        }
    }

    /// Execute a tool with confirmation handling and self-heal hints.
    /// Centralizes tool execution logic so strategies don't call `tool.execute()` directly.
    #[tracing::instrument(skip(self, json, tools, docker, confirmer, status_tx, trace), fields(tool = tool_name))]
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        json: &Value,
        tools: &ToolRegistry,
        docker: &DockerSession,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        let params = json.get("params").cloned().unwrap_or_default();

        let tool_span = trace.map(|t| {
            let input_preview = serde_json::to_string(&params).unwrap_or_default();
            let input_str = if input_preview.len() > 300 {
                &input_preview[..input_preview.floor_char_boundary(300)]
            } else {
                &input_preview
            };
            t.span(&format!("tool:{}", tool_name), Some(input_str))
        });

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
                    return Ok("The user denied this action. Try a different approach.".to_string());
                }
            }
        }

        let start = std::time::Instant::now();
        let result = tool.execute(docker, &params).await;
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

        // Record usage stats
        {
            let success = result.as_ref().map(|r| r.success).unwrap_or(false);
            let error_msg = match &result {
                Ok(r) if !r.success => Some(r.output.clone()),
                Err(e) => Some(e.to_string()),
                _ => None,
            };
            if let Err(e) =
                self.usage_store
                    .record(tool_name, success, duration_ms, error_msg.as_deref())
            {
                tracing::warn!("Failed to record tool usage: {}", e);
            }
            self.observer.log(
                ObserverCategory::ToolUsage,
                format!(
                    "{} {} ({:.0}ms)",
                    tool_name,
                    if success { "ok" } else { "fail" },
                    duration_ms
                ),
            );
        }

        let output = match result {
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
        };

        // End Langfuse tool span
        if let Some(s) = tool_span {
            let preview = match &output {
                Ok(o) => {
                    if o.len() > 500 {
                        format!("{}...", &o[..o.floor_char_boundary(500)])
                    } else {
                        o.clone()
                    }
                }
                Err(e) => format!("error: {}", e),
            };
            s.end(Some(&preview));
        }

        // Emit ToolRun event to frontend
        if let (Some(tx), Ok(ref out)) = (status_tx, &output) {
            let success = !out.starts_with("[tool error]");
            let _ = tx
                .send(CoreEvent::ToolRun {
                    tool: tool_name.to_string(),
                    result: out.clone(),
                    success,
                })
                .await;
        }

        output
    }
}
