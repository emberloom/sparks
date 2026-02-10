use async_trait::async_trait;

use crate::confirm;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::llm::{self, Message, OllamaClient};
use crate::tools::ToolRegistry;

use super::{LoopStrategy, TaskContract};

pub struct ReactStrategy;

#[async_trait]
impl LoopStrategy for ReactStrategy {
    async fn run(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &OllamaClient,
        max_steps: usize,
        sensitive_patterns: &[String],
    ) -> Result<String> {
        let system_prompt = build_system_prompt(contract, tools);
        let mut history: Vec<Message> = vec![
            Message::system(&system_prompt),
            Message::user(&contract.goal),
        ];

        for step in 0..max_steps {
            tracing::debug!(step, "ReAct step");

            let response = llm.chat(&history).await?;
            history.push(Message::assistant(&response));

            // Try to extract a tool call from the response
            let json = match llm::extract_json(&response) {
                Some(v) if v.get("tool").is_some() => v,
                _ => {
                    // No tool call — LLM is giving a final answer
                    tracing::info!(step, "ReAct complete (text response)");
                    return Ok(response);
                }
            };

            let tool_name = json["tool"].as_str().unwrap_or("");
            let params = json.get("params").cloned().unwrap_or_default();

            let tool = tools.get(tool_name)
                .ok_or_else(|| AthenaError::Tool(format!("Unknown tool: {}", tool_name)))?;

            // Confirmation for sensitive operations
            if tool.needs_confirmation() {
                let action_desc = format!(
                    "[{}] {}",
                    tool_name,
                    params.get("command")
                        .or_else(|| params.get("path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("(action)")
                );

                // Check sensitive patterns for extra warning
                let is_sensitive = if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
                    confirm::is_sensitive(cmd, sensitive_patterns)
                } else {
                    false
                };

                if is_sensitive {
                    eprintln!("🔴 SENSITIVE command detected!");
                }

                match confirm::confirm(&action_desc) {
                    Ok(true) => {} // approved
                    _ => {
                        // Denied — tell the LLM it was blocked
                        history.push(Message::user(
                            "The user denied this action. Try a different approach or explain what you need."
                        ));
                        continue;
                    }
                }
            }

            // Execute the tool
            let result = tool.execute(docker, &params).await;

            let tool_output = match result {
                Ok(r) => {
                    if r.success {
                        format!("[tool result]\n{}", r.output)
                    } else {
                        format!("[tool error]\n{}", r.output)
                    }
                }
                Err(e) => format!("[tool error]\n{}", e),
            };

            tracing::debug!(step, tool = tool_name, "Tool executed");
            history.push(Message::user(&tool_output));
        }

        Err(AthenaError::StepLimitExceeded(max_steps))
    }
}

fn build_system_prompt(contract: &TaskContract, tools: &ToolRegistry) -> String {
    let constraints = if contract.constraints.is_empty() {
        "None".to_string()
    } else {
        contract.constraints.iter()
            .map(|c| format!("- {}", c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
r#"You are an autonomous agent executing a task inside a Docker container.

CONTEXT: {}

AVAILABLE TOOLS:
{}

CONSTRAINTS:
{}

INSTRUCTIONS:
- To use a tool, respond with ONLY a JSON object: {{"tool": "<name>", "params": {{...}}}}
- Do NOT wrap the JSON in markdown code blocks. Output raw JSON only when calling a tool.
- After receiving tool results, you may call another tool or provide your final answer.
- When you have the answer, respond with plain text (no JSON).
- Be concise and efficient. Minimize the number of tool calls.
- If a tool call fails, try a different approach."#,
        contract.context,
        tools.descriptions(),
        constraints,
    )
}
