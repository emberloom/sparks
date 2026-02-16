pub mod code;
pub mod react;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::confirm::Confirmer;
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::langfuse::ActiveTrace;
use crate::llm::{self, ChatMessage, ChatResponse, LlmProvider, StreamEvent};
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
    /// Whether the VERIFY phase should generate tests for changes
    pub test_generation: bool,
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
        trace: Option<&ActiveTrace>,
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

const MAX_PRECHECK_STEPS: usize = 3;

/// Try to accomplish the task directly with available tools before entering the strategy.
/// Returns `Some(result)` if completed, `None` if the strategy should run.
pub async fn try_direct_completion(
    contract: &TaskContract,
    tools: &ToolRegistry,
    docker: &DockerSession,
    llm: &dyn LlmProvider,
    executor: &Executor,
    confirmer: &dyn Confirmer,
    status_tx: Option<&StatusSender>,
    trace: Option<&ActiveTrace>,
) -> Result<Option<String>> {
    // Only use this path when the LLM supports native tool calling
    if !llm.supports_tools() {
        return Ok(None);
    }

    let soul_section = match &contract.soul {
        Some(soul) => format!("{}\n\n", soul),
        None => String::new(),
    };

    let system_prompt = format!(
        r#"{soul}You have tools available. Determine if you can accomplish the following task directly.

IMPORTANT:
- Check your available tools FIRST. If a tool can handle the task (e.g., manage_tools for creating/editing dynamic tools, shell for running commands), use it immediately.
- Call ONLY the tool needed for the task. Do NOT explore, grep, or read files unless that IS the task.
- After a tool succeeds, respond with a summary that includes the tool's output. Do NOT call more tools.

If the task requires writing or modifying source code across multiple files, respond with:
{{"needs_strategy": true, "reason": "<why tools aren't sufficient>"}}

Otherwise, accomplish the task with your tools, then respond with a summary including the result."#,
        soul = soul_section,
    );

    // Pass ALL tool schemas — no whitelisting
    let schemas = tools.tool_schemas();

    let mut history: Vec<ChatMessage> = vec![
        ChatMessage::System(system_prompt),
        ChatMessage::User(contract.goal.clone()),
    ];

    let use_streaming = llm.supports_streaming();

    for step in 0..MAX_PRECHECK_STEPS {
        tracing::debug!(step, "precheck step");

        let (text_accum, tool_calls, _usage) = if use_streaming {
            let mut rx = llm.chat_with_tools_stream(&history, &schemas).await?;
            // Don't forward stream chunks — precheck text is internal reasoning, not user-facing
            consume_precheck_stream(&mut rx, None).await
        } else {
            let (response, usage) = llm.chat_with_tools(&history, &schemas).await?;
            match response {
                ChatResponse::ToolCalls { tool_calls, text } => {
                    (text.unwrap_or_default(), tool_calls, usage)
                }
                ChatResponse::Text(text) => (text, vec![], usage),
            }
        };

        if !tool_calls.is_empty() {
            // LLM wants to use tools — execute them
            let text = if text_accum.is_empty() {
                None
            } else {
                Some(text_accum)
            };
            history.push(ChatMessage::Assistant {
                content: text,
                tool_calls: Some(tool_calls.clone()),
            });

            for tc in &tool_calls {
                if let Some(tx) = status_tx {
                    let _ = tx
                        .send(CoreEvent::Status(format!("Precheck: {} ...", tc.name)))
                        .await;
                }

                let json = serde_json::json!({
                    "tool": tc.name,
                    "params": tc.arguments,
                });
                let result = executor
                    .execute_tool(&tc.name, &json, tools, docker, confirmer, status_tx, trace)
                    .await;
                let output = result.unwrap_or_else(|e| format!("[tool error]\n{}", e));
                tracing::debug!(step, tool = %tc.name, "precheck tool executed");
                history.push(ChatMessage::Tool {
                    tool_call_id: tc.id.clone(),
                    content: output,
                });
            }
        } else {
            // Pure text response — check if it needs strategy fallback
            if let Some(json) = llm::extract_json(&text_accum) {
                if json
                    .get("needs_strategy")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let reason = json
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(reason, "precheck: task needs strategy");
                    return Ok(None);
                }
            }
            // Task completed directly
            tracing::info!(step, "precheck: task completed directly");
            return Ok(Some(text_accum));
        }
    }

    // Step limit hit — if tools were used, ask for a final summary instead of dropping results
    let tools_were_used = history
        .iter()
        .any(|m| matches!(m, ChatMessage::Tool { .. }));
    if tools_were_used {
        tracing::info!("precheck: step limit reached, requesting summary");
        history.push(ChatMessage::User(
            "Summarize what you accomplished. Include any tool output in your response."
                .to_string(),
        ));
        let (response, _) = llm.chat_with_tools(&history, &[]).await?;
        let summary = match response {
            ChatResponse::Text(text) => text,
            ChatResponse::ToolCalls { text, .. } => text.unwrap_or_default(),
        };
        if !summary.is_empty() {
            return Ok(Some(summary));
        }
    }

    // No tools were used or summary empty — fall through to strategy
    tracing::info!("precheck: falling through to strategy");
    Ok(None)
}

/// Consume a streaming response for the precheck phase.
async fn consume_precheck_stream(
    rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    status_tx: Option<&StatusSender>,
) -> (
    String,
    Vec<crate::llm::ToolCall>,
    Option<crate::llm::TokenUsage>,
) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                if let Some(tx) = status_tx {
                    let _ = tx.send(CoreEvent::StreamChunk(delta.clone())).await;
                }
                text.push_str(&delta);
            }
            StreamEvent::ToolCallComplete(tc) => {
                tool_calls.push(tc);
            }
            StreamEvent::Usage(u) => {
                usage = Some(u);
            }
            StreamEvent::Done => break,
        }
    }

    (text, tool_calls, usage)
}
