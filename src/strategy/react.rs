use async_trait::async_trait;

use crate::confirm::Confirmer;
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::langfuse::ActiveTrace;
use crate::llm::{
    ChatMessage, ChatResponse, LlmProvider, Message, StreamEvent, TokenBudget, ToolCall,
};
use crate::tools::ToolRegistry;

use super::{materialize_tool_result, LoopStrategy, StatusSender, TaskContract};

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
        status_tx: Option<&StatusSender>,
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        if llm.supports_tools() {
            self.run_native(
                contract, tools, docker, llm, max_steps, executor, confirmer, status_tx, trace,
            )
            .await
        } else {
            self.run_text_fallback(
                contract, tools, docker, llm, max_steps, executor, confirmer, trace,
            )
            .await
        }
    }
}

impl ReactStrategy {
    /// Native function calling path: uses `ChatMessage` + `chat_with_tools()`.
    /// When streaming is supported, text deltas are forwarded to the frontend in real time.
    async fn run_native(
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
    ) -> Result<String> {
        let system_prompt = build_system_prompt_native(contract);
        let schemas = tools.tool_schemas();

        let mut history: Vec<ChatMessage> = vec![
            ChatMessage::System(system_prompt),
            ChatMessage::User(contract.goal.clone()),
        ];

        let mut budget = TokenBudget::new(llm.context_window());
        let use_streaming = llm.supports_streaming();
        let model_name = llm.provider_name();

        for step in 0..max_steps {
            tracing::debug!(step, path = "native", stream = use_streaming, "ReAct step");

            let gen =
                trace.map(|t| t.generation(&format!("react_step_{}", step), model_name, None));

            if use_streaming {
                // Streaming path: consume StreamEvents
                let mut rx = llm.chat_with_tools_stream(&history, &schemas).await?;

                let mut text_accum = String::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut usage = None;

                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::TextDelta(delta) => {
                            // Forward to frontend for real-time display
                            if let Some(tx) = status_tx {
                                let _ = tx.send(CoreEvent::StreamChunk(delta.clone())).await;
                            }
                            text_accum.push_str(&delta);
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

                // End generation with response info
                if let Some(g) = gen {
                    let (pt, ct) = usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens))
                        .unwrap_or((0, 0));
                    let out = if !tool_calls.is_empty() {
                        let names: Vec<&str> =
                            tool_calls.iter().map(|tc| tc.name.as_str()).collect();
                        format!("tools: {}", names.join(", "))
                    } else {
                        lf_truncate(&text_accum, 500)
                    };
                    g.end(Some(&out), pt, ct);
                }

                // Record token usage
                if let Some(ref u) = usage {
                    budget.record_usage(u);
                    tracing::debug!(
                        utilization = format!("{:.1}%", budget.utilization() * 100.0),
                        prompt_tokens = u.prompt_tokens,
                        call_count = budget.call_count,
                        "Token budget"
                    );
                    if budget.needs_compression(0.80) {
                        tracing::info!(
                            utilization = format!("{:.1}%", budget.utilization() * 100.0),
                            "Context window >80%, compressing history"
                        );
                        compress_history(&mut history);
                    }
                }

                if !tool_calls.is_empty() {
                    let text = if text_accum.is_empty() {
                        None
                    } else {
                        Some(text_accum)
                    };
                    history.push(ChatMessage::Assistant {
                        content: text,
                        tool_calls: Some(tool_calls.clone()),
                    });

                    // Execute tool calls in parallel
                    let futs: Vec<_> = tool_calls
                        .iter()
                        .map(|tc| async move {
                            let json = serde_json::json!({
                                "tool": tc.name,
                                "params": tc.arguments,
                            });
                            let result = executor
                                .execute_tool(
                                    &tc.name, &json, tools, docker, confirmer, status_tx, trace,
                                )
                                .await;
                            (tc, result)
                        })
                        .collect();

                    let results = futures::future::join_all(futs).await;
                    for (tc, result) in results {
                        let output = materialize_tool_result(result)?;
                        tracing::debug!(step, tool = %tc.name, path = "native", "Tool executed");
                        history.push(ChatMessage::Tool {
                            tool_call_id: tc.id.clone(),
                            content: output,
                        });
                    }
                } else {
                    // Pure text response — done
                    tracing::info!(step, path = "native", "ReAct complete (streamed text)");
                    return Ok(text_accum);
                }
            } else {
                // Non-streaming fallback path
                let (response, usage) = llm.chat_with_tools(&history, &schemas).await?;

                // End generation
                if let Some(g) = gen {
                    let (pt, ct) = usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens))
                        .unwrap_or((0, 0));
                    let out = match &response {
                        ChatResponse::ToolCalls { tool_calls, .. } => {
                            let names: Vec<&str> =
                                tool_calls.iter().map(|tc| tc.name.as_str()).collect();
                            format!("tools: {}", names.join(", "))
                        }
                        ChatResponse::Text(t) => lf_truncate(t, 500),
                    };
                    g.end(Some(&out), pt, ct);
                }

                if let Some(ref u) = usage {
                    budget.record_usage(u);
                    tracing::debug!(
                        utilization = format!("{:.1}%", budget.utilization() * 100.0),
                        prompt_tokens = u.prompt_tokens,
                        call_count = budget.call_count,
                        "Token budget"
                    );
                    if budget.needs_compression(0.80) {
                        tracing::info!(
                            utilization = format!("{:.1}%", budget.utilization() * 100.0),
                            "Context window >80%, compressing history"
                        );
                        compress_history(&mut history);
                    }
                }

                match response {
                    ChatResponse::ToolCalls { tool_calls, text } => {
                        history.push(ChatMessage::Assistant {
                            content: text,
                            tool_calls: Some(tool_calls.clone()),
                        });

                        let futs: Vec<_> = tool_calls
                            .iter()
                            .map(|tc| async move {
                                let json = serde_json::json!({
                                    "tool": tc.name,
                                    "params": tc.arguments,
                                });
                                let result = executor
                                    .execute_tool(
                                        &tc.name, &json, tools, docker, confirmer, status_tx, trace,
                                    )
                                    .await;
                                (tc, result)
                            })
                            .collect();

                        let results = futures::future::join_all(futs).await;
                        for (tc, result) in results {
                            let output = materialize_tool_result(result)?;
                            tracing::debug!(step, tool = %tc.name, path = "native", "Tool executed");
                            history.push(ChatMessage::Tool {
                                tool_call_id: tc.id.clone(),
                                content: output,
                            });
                        }
                    }
                    ChatResponse::Text(text) => {
                        tracing::info!(step, path = "native", "ReAct complete (text response)");
                        return Ok(text);
                    }
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
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        let system_prompt = build_system_prompt(contract, tools);
        let mut history: Vec<Message> = vec![
            Message::system(&system_prompt),
            Message::user(&contract.goal),
        ];
        let model_name = llm.provider_name();

        for step in 0..max_steps {
            tracing::debug!(step, path = "text", "ReAct step");

            let gen =
                trace.map(|t| t.generation(&format!("react_step_{}", step), model_name, None));

            let response = llm.chat(&history).await?;

            if let Some(g) = gen {
                g.end(Some(&lf_truncate(&response, 500)), 0, 0);
            }

            history.push(Message::assistant(&response));

            // Try to extract a tool call from the response
            let json = match parse_text_tool_envelope(&response) {
                Some(v) => v,
                None => {
                    // No valid tool envelope — LLM is giving a final answer.
                    tracing::info!(step, path = "text", "ReAct complete (text response)");
                    return Ok(response);
                }
            };

            let tool_name = json["tool"].as_str().unwrap_or("");

            let tool_output = executor
                .execute_tool(tool_name, &json, tools, docker, confirmer, None, trace)
                .await?;

            tracing::debug!(step, tool = tool_name, path = "text", "Tool executed");
            history.push(Message::user(&tool_output));
        }

        Err(AthenaError::StepLimitExceeded(max_steps))
    }
}

fn parse_text_tool_envelope(response: &str) -> Option<serde_json::Value> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let tool = json.get("tool")?.as_str()?;
    if tool.is_empty() {
        return None;
    }
    let params = json.get("params")?;
    if !params.is_object() {
        return None;
    }
    Some(json)
}

/// Truncate a string at a UTF-8 boundary for Langfuse output fields.
fn lf_truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::parse_text_tool_envelope;
    use serde_json::json;

    #[test]
    fn parse_text_tool_envelope_accepts_valid_json() {
        let payload = json!({
            "tool": "grep",
            "params": { "pattern": "todo" }
        });
        let parsed = parse_text_tool_envelope(&payload.to_string());
        assert!(parsed.is_some());
    }

    #[test]
    fn parse_text_tool_envelope_rejects_prose_without_json() {
        let parsed = parse_text_tool_envelope("Sure, I'll do that next.");
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_text_tool_envelope_rejects_malformed_json() {
        let parsed = parse_text_tool_envelope("{\"tool\": \"grep\", \"params\": }");
        assert!(parsed.is_none());
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

/// Compress conversation history when context window fills up.
/// Preserves: system prompt (index 0), initial user goal (index 1), last 6 messages.
/// Replaces middle messages with a single summary.
pub fn compress_history(history: &mut Vec<ChatMessage>) {
    let preserve_tail = 6; // last 3 round-trips
    let preserve_head = 2; // system + initial user goal

    if history.len() <= preserve_head + preserve_tail {
        return; // nothing to compress
    }

    let middle_end = history.len() - preserve_tail;

    // Build a summary of the middle messages
    let mut summary_parts: Vec<String> = Vec::new();
    for msg in &history[preserve_head..middle_end] {
        match msg {
            ChatMessage::Assistant {
                tool_calls: Some(tcs),
                ..
            } => {
                let names: Vec<&str> = tcs.iter().map(|tc| tc.name.as_str()).collect();
                summary_parts.push(format!("Called: {}", names.join(", ")));
            }
            ChatMessage::Tool { content, .. } => {
                let preview: String = content.chars().take(80).collect();
                summary_parts.push(format!("Result: {}", preview));
            }
            ChatMessage::Assistant {
                content: Some(text),
                ..
            } => {
                let preview: String = text.chars().take(80).collect();
                summary_parts.push(format!("Said: {}", preview));
            }
            _ => {}
        }
    }

    let summary = format!(
        "[Compressed {} messages]\n{}",
        middle_end - preserve_head,
        summary_parts.join("\n")
    );

    // Replace middle with a single user message summary
    let tail: Vec<ChatMessage> = history.split_off(middle_end);
    history.truncate(preserve_head);
    history.push(ChatMessage::User(summary));
    history.extend(tail);

    tracing::debug!(new_len = history.len(), "History compressed");
}
