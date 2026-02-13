use async_trait::async_trait;

use crate::confirm::Confirmer;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::llm::{self, ChatMessage, ChatResponse, LlmProvider, Message};
use crate::tools::ToolRegistry;

use super::{LoopStrategy, StatusSender, TaskContract};

pub struct ReactStrategy;

#[async_trait]
impl LoopStrategy for ReactStrategy {
    async fn run(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        max_steps: usize,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        _status_tx: Option<&StatusSender>,
    ) -> Result<String> {
        if llm.supports_tools() {
            self.run_native(contract, tools, docker, llm, max_steps, executor, confirmer)
                .await
        } else {
            self.run_text_fallback(contract, tools, docker, llm, max_steps, executor, confirmer)
                .await
        }
    }
}

impl ReactStrategy {
    /// Native function calling path: uses `ChatMessage` + `chat_with_tools()`.
    async fn run_native(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        max_steps: usize,
        executor: &Executor,
        confirmer: &dyn Confirmer,
    ) -> Result<String> {
        let system_prompt = build_system_prompt_native(contract);
        let schemas = tools.tool_schemas();

        let mut history: Vec<ChatMessage> = vec![
            ChatMessage::System(system_prompt),
            ChatMessage::User(contract.goal.clone()),
        ];

        for step in 0..max_steps {
            tracing::debug!(step, path = "native", "ReAct step");

            let response = llm.chat_with_tools(&history, &schemas).await?;

            match response {
                ChatResponse::ToolCalls { tool_calls, text } => {
                    // Push assistant message with tool_calls
                    history.push(ChatMessage::Assistant {
                        content: text,
                        tool_calls: Some(tool_calls.clone()),
                    });

                    // Execute each tool call and push results
                    for tc in &tool_calls {
                        let json = serde_json::json!({
                            "tool": tc.name,
                            "params": tc.arguments,
                        });
                        let tool_output = executor
                            .execute_tool(&tc.name, &json, tools, docker, confirmer)
                            .await?;

                        tracing::debug!(step, tool = %tc.name, path = "native", "Tool executed");
                        history.push(ChatMessage::Tool {
                            tool_call_id: tc.id.clone(),
                            content: tool_output,
                        });
                    }
                }
                ChatResponse::Text(text) => {
                    tracing::info!(step, path = "native", "ReAct complete (text response)");
                    return Ok(text);
                }
            }
        }

        Err(AthenaError::StepLimitExceeded(max_steps))
    }

    /// Text fallback path: existing implementation using `Message` + `chat()` + `extract_json()`.
    async fn run_text_fallback(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        max_steps: usize,
        executor: &Executor,
        confirmer: &dyn Confirmer,
    ) -> Result<String> {
        let system_prompt = build_system_prompt(contract, tools);
        let mut history: Vec<Message> = vec![
            Message::system(&system_prompt),
            Message::user(&contract.goal),
        ];

        for step in 0..max_steps {
            tracing::debug!(step, path = "text", "ReAct step");

            let response = llm.chat(&history).await?;
            history.push(Message::assistant(&response));

            // Try to extract a tool call from the response
            let json = match llm::extract_json(&response) {
                Some(v) if v.get("tool").is_some() => v,
                _ => {
                    // No tool call — LLM is giving a final answer.
                    tracing::info!(step, path = "text", "ReAct complete (text response)");
                    return Ok(response);
                }
            };

            let tool_name = json["tool"].as_str().unwrap_or("");

            let tool_output = executor
                .execute_tool(tool_name, &json, tools, docker, confirmer)
                .await?;

            tracing::debug!(step, tool = tool_name, path = "text", "Tool executed");
            history.push(Message::user(&tool_output));
        }

        Err(AthenaError::StepLimitExceeded(max_steps))
    }
}

/// System prompt for native function calling — no embedded tool descriptions needed.
fn build_system_prompt_native(contract: &TaskContract) -> String {
    let constraints = if contract.constraints.is_empty() {
        "None".to_string()
    } else {
        contract
            .constraints
            .iter()
            .map(|c| format!("- {}", c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let soul_section = match &contract.soul {
        Some(soul) => format!("{}\n\n", soul),
        None => String::new(),
    };

    format!(
        r#"{}You are an autonomous agent executing a task inside a Docker container.

CONTEXT: {}

CONSTRAINTS:
{}

INSTRUCTIONS:
- Use your tools to accomplish the task. Call tools as needed.
- After receiving tool results, you may call another tool or provide your final answer.
- When the task is FULLY DONE, respond with a brief summary in plain text. This is your final answer.
- Be concise and efficient. Minimize the number of tool calls.
- If a tool call fails, try a different approach.
- CRITICAL: You are an EXECUTOR, not a planner. Do NOT describe what you would do — actually DO it using your tools."#,
        soul_section, contract.context, constraints,
    )
}

/// System prompt for text fallback — includes full tool descriptions.
fn build_system_prompt(contract: &TaskContract, tools: &ToolRegistry) -> String {
    let constraints = if contract.constraints.is_empty() {
        "None".to_string()
    } else {
        contract
            .constraints
            .iter()
            .map(|c| format!("- {}", c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let soul_section = match &contract.soul {
        Some(soul) => format!("{}\n\n", soul),
        None => String::new(),
    };

    let tools_section = match &contract.tools_doc {
        Some(doc) => format!("\n\nTOOL REFERENCE:\n{}", doc),
        None => String::new(),
    };

    format!(
r#"{}You are an autonomous agent executing a task inside a Docker container.

CONTEXT: {}

AVAILABLE TOOLS:
{}{}

CONSTRAINTS:
{}

INSTRUCTIONS:
- To use a tool, respond with ONLY a JSON object: {{"tool": "<name>", "params": {{...}}}}
- Do NOT wrap the JSON in markdown code blocks. Output raw JSON only when calling a tool.
- After receiving tool results, you may call another tool or provide your final answer.
- When the task is FULLY DONE, respond with a brief summary in plain text (no JSON). This is your final answer.
- Be concise and efficient. Minimize the number of tool calls.
- If a tool call fails, try a different approach.
- CRITICAL: You are an EXECUTOR, not a planner. Do NOT describe what you would do — actually DO it using your tools."#,
        soul_section,
        contract.context,
        tools.descriptions(),
        tools_section,
        constraints,
    )
}
