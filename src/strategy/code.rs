use async_trait::async_trait;
use tracing::Instrument;

use crate::confirm::Confirmer;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::core::CoreEvent;
use crate::llm::{self, ChatMessage, ChatResponse, LlmProvider, Message, StreamEvent, TokenBudget, ToolCall, ToolSchema};
use crate::tools::ToolRegistry;

use super::{LoopStrategy, StatusSender, TaskContract};

/// Read-only tools allowed in the EXPLORE phase
const EXPLORE_TOOLS: &[&str] = &["file_read", "grep", "glob", "codebase_map", "shell", "diff", "git", "gh"];
/// Tools allowed in the VERIFY phase (read-only + lint)
const VERIFY_TOOLS: &[&str] = &["file_read", "grep", "glob", "shell", "lint", "diff", "git", "gh"];
/// Coding CLI tools for the EXECUTE phase
const CODING_TOOLS: &[&str] = &["claude_code", "codex", "opencode"];

const MAX_EXPLORE_STEPS: usize = 5;
const MAX_VERIFY_STEPS: usize = 5;

struct ExplorationResult {
    plan: String,
    context: String,
    files: String,
}

pub struct CodeStrategy;

#[async_trait]
impl LoopStrategy for CodeStrategy {
    async fn run(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        _max_steps: usize,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<String> {
        let use_native = llm.supports_tools();

        // Phase 1: EXPLORE
        tracing::info!("CodeStrategy: starting EXPLORE phase");
        send_status(status_tx, "Exploring codebase...").await;
        let exploration = if use_native {
            self.explore_native(contract, tools, docker, llm, executor, confirmer, status_tx)
                .instrument(tracing::info_span!("explore"))
                .await?
        } else {
            self.explore_text_fallback(contract, tools, docker, llm, executor, confirmer, status_tx)
                .instrument(tracing::info_span!("explore"))
                .await?
        };

        // Phase 2: EXECUTE (unchanged — calls CLI tool directly)
        tracing::info!("CodeStrategy: starting EXECUTE phase");
        send_status(status_tx, "Executing code changes...").await;
        let exec_result = self
            .execute_code(contract, tools, docker, llm, &exploration)
            .instrument(tracing::info_span!("execute"))
            .await?;

        // Phase 3: VERIFY
        tracing::info!("CodeStrategy: starting VERIFY phase");
        send_status(status_tx, "Verifying changes...").await;
        let summary = if use_native {
            self.verify_native(contract, tools, docker, llm, &exec_result, executor, confirmer, status_tx)
                .instrument(tracing::info_span!("verify"))
                .await?
        } else {
            self.verify_text_fallback(contract, tools, docker, llm, &exec_result, executor, confirmer, status_tx)
                .instrument(tracing::info_span!("verify"))
                .await?
        };

        Ok(summary)
    }
}

impl CodeStrategy {
    // ── EXPLORE: native path ─────────────────────────────────────────

    async fn explore_native(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<ExplorationResult> {
        let system_prompt = build_explore_prompt_native(contract);
        let schemas = phase_schemas(tools, EXPLORE_TOOLS);

        let mut history: Vec<ChatMessage> = vec![
            ChatMessage::System(system_prompt),
            ChatMessage::User(contract.goal.clone()),
        ];

        let mut budget = TokenBudget::new(llm.context_window());
        let use_streaming = llm.supports_streaming();

        for step in 0..MAX_EXPLORE_STEPS {
            tracing::debug!(step, path = "native", "EXPLORE step");

            // Get response (streaming or non-streaming)
            let (text_accum, tool_calls, usage) = if use_streaming {
                let mut rx = llm.chat_with_tools_stream(&history, &schemas).await?;
                consume_stream(&mut rx, status_tx).await
            } else {
                let (response, usage) = llm.chat_with_tools(&history, &schemas).await?;
                match response {
                    ChatResponse::ToolCalls { tool_calls, text } => {
                        (text.unwrap_or_default(), tool_calls, usage)
                    }
                    ChatResponse::Text(text) => (text, vec![], usage),
                }
            };

            if let Some(ref u) = usage {
                budget.record_usage(u);
                if budget.needs_compression(0.80) {
                    tracing::info!(
                        utilization = format!("{:.1}%", budget.utilization() * 100.0),
                        "EXPLORE: context >80%, compressing history"
                    );
                    super::react::compress_history(&mut history);
                }
            }

            if !tool_calls.is_empty() {
                // Check if the text portion contains a plan
                if !text_accum.is_empty() {
                    if let Some(plan) = extract_plan(&text_accum) {
                        tracing::info!(step, path = "native", "EXPLORE complete — got plan from text");
                        return Ok(plan);
                    }
                }

                let text = if text_accum.is_empty() { None } else { Some(text_accum) };
                history.push(ChatMessage::Assistant {
                    content: text,
                    tool_calls: Some(tool_calls.clone()),
                });

                // Split into allowed and disallowed tool calls
                let (allowed, disallowed): (Vec<_>, Vec<_>) = tool_calls.iter()
                    .partition(|tc| EXPLORE_TOOLS.contains(&tc.name.as_str()));

                for tc in &disallowed {
                    history.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: format!(
                            "Tool '{}' is not allowed in the exploration phase. Use only: {}",
                            tc.name,
                            EXPLORE_TOOLS.join(", ")
                        ),
                    });
                }

                // Execute allowed tool calls in parallel
                for tc in &allowed {
                    send_status(status_tx, &format!("Exploring: {} ...", tc.name)).await;
                }
                let futs: Vec<_> = allowed.iter().map(|tc| async move {
                    let json = serde_json::json!({
                        "tool": tc.name,
                        "params": tc.arguments,
                    });
                    let result = executor
                        .execute_tool(&tc.name, &json, tools, docker, confirmer, status_tx)
                        .await;
                    (*tc, result)
                }).collect();

                let results = futures::future::join_all(futs).await;
                for (tc, result) in results {
                    let output = result.unwrap_or_else(|e| format!("[tool error]\n{}", e));
                    tracing::debug!(step, tool = %tc.name, path = "native", "EXPLORE tool executed");
                    history.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: output,
                    });
                }
            } else {
                // Pure text response
                if let Some(plan) = extract_plan(&text_accum) {
                    tracing::info!(step, path = "native", "EXPLORE complete — got plan");
                    return Ok(plan);
                }
                // No plan — nudge
                history.push(ChatMessage::Assistant {
                    content: Some(text_accum),
                    tool_calls: None,
                });
                history.push(ChatMessage::User(
                    "You need to either call a read-only tool to explore, or output your plan as JSON:\n\
                     {\"plan\": \"<step-by-step plan>\", \"context\": \"<what you learned>\", \"files\": \"<key file paths and excerpts>\"}\n\
                     Keep exploring if you need more context, then output the plan JSON.".to_string(),
                ));
            }
        }

        // Force plan
        tracing::warn!("EXPLORE phase (native) hit step limit, requesting plan");
        history.push(ChatMessage::User(
            "You've used all exploration steps. Output your plan NOW as JSON:\n\
             {\"plan\": \"...\", \"context\": \"...\", \"files\": \"...\"}".to_string(),
        ));
        let (response, _) = llm.chat_with_tools(&history, &[]).await?;
        if let ChatResponse::Text(text) = &response {
            if let Some(plan) = extract_plan(text) {
                return Ok(plan);
            }
        }

        Ok(ExplorationResult {
            plan: contract.goal.clone(),
            context: match response {
                ChatResponse::Text(t) => t,
                _ => String::new(),
            },
            files: String::new(),
        })
    }

    // ── EXPLORE: text fallback ───────────────────────────────────────

    async fn explore_text_fallback(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<ExplorationResult> {
        let system_prompt = build_explore_prompt(contract, tools);
        let mut history = vec![
            Message::system(&system_prompt),
            Message::user(&contract.goal),
        ];

        for step in 0..MAX_EXPLORE_STEPS {
            tracing::debug!(step, path = "text", "EXPLORE step");

            let response = llm.chat(&history).await?;
            history.push(Message::assistant(&response));

            if let Some(plan) = extract_plan(&response) {
                tracing::info!(step, path = "text", "EXPLORE complete — got plan");
                return Ok(plan);
            }

            let json = match llm::extract_json(&response) {
                Some(v) if v.get("tool").is_some() => v,
                _ => {
                    history.push(Message::user(
                        "You need to either call a read-only tool to explore, or output your plan as JSON:\n\
                         {\"plan\": \"<step-by-step plan>\", \"context\": \"<what you learned>\", \"files\": \"<key file paths and excerpts>\"}\n\
                         Keep exploring if you need more context, then output the plan JSON.",
                    ));
                    continue;
                }
            };

            let tool_name = json["tool"].as_str().unwrap_or("");

            if !EXPLORE_TOOLS.contains(&tool_name) {
                history.push(Message::user(&format!(
                    "Tool '{}' is not allowed in the exploration phase. Use only: {}",
                    tool_name,
                    EXPLORE_TOOLS.join(", ")
                )));
                continue;
            }

            send_status(status_tx, &format!("Exploring: {} ...", tool_name)).await;

            let tool_output = executor
                .execute_tool(tool_name, &json, tools, docker, confirmer, status_tx)
                .await?;

            tracing::debug!(step, tool = tool_name, path = "text", "EXPLORE tool executed");
            history.push(Message::user(&tool_output));
        }

        tracing::warn!("EXPLORE phase (text) hit step limit, requesting plan");
        history.push(Message::user(
            "You've used all exploration steps. Output your plan NOW as JSON:\n\
             {\"plan\": \"...\", \"context\": \"...\", \"files\": \"...\"}",
        ));
        let response = llm.chat(&history).await?;
        if let Some(plan) = extract_plan(&response) {
            return Ok(plan);
        }

        Ok(ExplorationResult {
            plan: contract.goal.clone(),
            context: response,
            files: String::new(),
        })
    }

    /// Phase 2: Call a coding CLI tool with the enriched plan from Phase 1.
    /// Unchanged — calls CLI tool directly, no LLM tool loop.
    async fn execute_code(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        exploration: &ExplorationResult,
    ) -> Result<String> {
        let coding_tool_name = contract
            .cli_tool_preference
            .as_deref()
            .filter(|pref| tools.get(pref).is_some())
            .or_else(|| {
                CODING_TOOLS
                    .iter()
                    .find(|&&name| tools.get(name).is_some())
                    .copied()
            })
            .ok_or_else(|| {
                AthenaError::Tool(
                    "No coding CLI tool available (need claude_code, codex, or opencode)".into(),
                )
            })?;

        let tool = tools.get(coding_tool_name).unwrap();

        let mut prompt_parts = Vec::new();

        if !exploration.context.is_empty() {
            prompt_parts.push(format!("CODEBASE CONTEXT:\n{}", exploration.context));
        }

        prompt_parts.push(format!("TASK:\n{}", exploration.plan));

        if !contract.constraints.is_empty() {
            prompt_parts.push(format!(
                "CONSTRAINTS:\n{}",
                contract
                    .constraints
                    .iter()
                    .map(|c| format!("- {}", c))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        let full_prompt = prompt_parts.join("\n\n");

        let params = serde_json::json!({
            "prompt": full_prompt,
            "context": contract.context,
            "files": exploration.files,
        });

        tracing::info!(tool = coding_tool_name, "EXECUTE: calling coding tool");
        let result = tool.execute(docker, &params).await?;

        if result.success {
            tracing::info!(tool = coding_tool_name, "EXECUTE: coding tool succeeded");
            return Ok(result.output);
        }

        tracing::warn!(
            tool = coding_tool_name,
            "EXECUTE: coding tool failed, attempting retry"
        );

        let retry_messages = vec![
            Message::system(
                "You are helping with a coding task. The coding tool failed. \
                 Analyze the error and produce a revised, more detailed prompt that addresses the issue.",
            ),
            Message::user(&format!(
                "Original prompt:\n{}\n\nError output:\n{}\n\n\
                 Provide a revised prompt that addresses the error. Output ONLY the revised prompt text.",
                full_prompt, result.output
            )),
        ];

        let revised_prompt = llm.chat(&retry_messages).await?;

        let retry_params = serde_json::json!({
            "prompt": revised_prompt,
            "context": format!(
                "{}\n\nPrevious attempt failed with:\n{}",
                contract.context, result.output
            ),
            "files": exploration.files,
        });

        tracing::info!(tool = coding_tool_name, "EXECUTE: retrying with revised prompt");
        let retry_result = tool.execute(docker, &retry_params).await?;

        Ok(retry_result.output)
    }

    // ── VERIFY: native path ──────────────────────────────────────────

    async fn verify_native(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        exec_result: &str,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<String> {
        let system_prompt = build_verify_prompt_native(contract, exec_result);
        let schemas = phase_schemas(tools, VERIFY_TOOLS);

        let mut history: Vec<ChatMessage> = vec![
            ChatMessage::System(system_prompt),
            ChatMessage::User(
                "Verify the changes are correct. Read modified files, run tests or \
                 compilation checks, and report what was done and any issues."
                    .to_string(),
            ),
        ];

        let mut budget = TokenBudget::new(llm.context_window());
        let use_streaming = llm.supports_streaming();

        for step in 0..MAX_VERIFY_STEPS {
            tracing::debug!(step, path = "native", "VERIFY step");

            // Get response (streaming or non-streaming)
            let (text_accum, tool_calls, usage) = if use_streaming {
                let mut rx = llm.chat_with_tools_stream(&history, &schemas).await?;
                consume_stream(&mut rx, status_tx).await
            } else {
                let (response, usage) = llm.chat_with_tools(&history, &schemas).await?;
                match response {
                    ChatResponse::ToolCalls { tool_calls, text } => {
                        (text.unwrap_or_default(), tool_calls, usage)
                    }
                    ChatResponse::Text(text) => (text, vec![], usage),
                }
            };

            if let Some(ref u) = usage {
                budget.record_usage(u);
                if budget.needs_compression(0.80) {
                    tracing::info!(
                        utilization = format!("{:.1}%", budget.utilization() * 100.0),
                        "VERIFY: context >80%, compressing history"
                    );
                    super::react::compress_history(&mut history);
                }
            }

            if !tool_calls.is_empty() {
                let text = if text_accum.is_empty() { None } else { Some(text_accum) };
                history.push(ChatMessage::Assistant {
                    content: text,
                    tool_calls: Some(tool_calls.clone()),
                });

                // Split into allowed and disallowed tool calls
                let (allowed, disallowed): (Vec<_>, Vec<_>) = tool_calls.iter()
                    .partition(|tc| VERIFY_TOOLS.contains(&tc.name.as_str()));

                for tc in &disallowed {
                    history.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: format!(
                            "Tool '{}' is not allowed in the verification phase. Use only: {}",
                            tc.name,
                            VERIFY_TOOLS.join(", ")
                        ),
                    });
                }

                // Execute allowed tool calls in parallel
                for tc in &allowed {
                    send_status(status_tx, &format!("Verifying: {} ...", tc.name)).await;
                }
                let futs: Vec<_> = allowed.iter().map(|tc| async move {
                    let json = serde_json::json!({
                        "tool": tc.name,
                        "params": tc.arguments,
                    });
                    let result = executor
                        .execute_tool(&tc.name, &json, tools, docker, confirmer, status_tx)
                        .await;
                    (*tc, result)
                }).collect();

                let results = futures::future::join_all(futs).await;
                for (tc, result) in results {
                    let output = result.unwrap_or_else(|e| format!("[tool error]\n{}", e));
                    tracing::debug!(step, tool = %tc.name, path = "native", "VERIFY tool executed");
                    history.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: output,
                    });
                }
            } else {
                // Pure text response — verification complete
                tracing::info!(step, path = "native", "VERIFY complete");
                return Ok(text_accum);
            }
        }

        // Ran out of steps — request summary
        history.push(ChatMessage::User(
            "Verification step limit reached. Provide your final summary now.".to_string(),
        ));
        let (response, _) = llm.chat_with_tools(&history, &[]).await?;
        match response {
            ChatResponse::Text(text) => Ok(text),
            ChatResponse::ToolCalls { text, .. } => Ok(text.unwrap_or_default()),
        }
    }

    // ── VERIFY: text fallback ────────────────────────────────────────

    async fn verify_text_fallback(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        exec_result: &str,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
    ) -> Result<String> {
        let system_prompt = build_verify_prompt(contract, tools, exec_result);
        let mut history = vec![
            Message::system(&system_prompt),
            Message::user(
                "Verify the changes are correct. Read modified files, run tests or \
                 compilation checks, and report what was done and any issues.",
            ),
        ];

        for step in 0..MAX_VERIFY_STEPS {
            tracing::debug!(step, path = "text", "VERIFY step");

            let response = llm.chat(&history).await?;
            history.push(Message::assistant(&response));

            let json = match llm::extract_json(&response) {
                Some(v) if v.get("tool").is_some() => v,
                _ => {
                    tracing::info!(step, path = "text", "VERIFY complete");
                    return Ok(response);
                }
            };

            let tool_name = json["tool"].as_str().unwrap_or("");

            if !VERIFY_TOOLS.contains(&tool_name) {
                history.push(Message::user(&format!(
                    "Tool '{}' is not allowed in the verification phase. Use only: {}",
                    tool_name,
                    VERIFY_TOOLS.join(", ")
                )));
                continue;
            }

            send_status(status_tx, &format!("Verifying: {} ...", tool_name)).await;

            let tool_output = executor
                .execute_tool(tool_name, &json, tools, docker, confirmer, status_tx)
                .await?;

            tracing::debug!(step, tool = tool_name, path = "text", "VERIFY tool executed");
            history.push(Message::user(&tool_output));
        }

        history.push(Message::user(
            "Verification step limit reached. Provide your final summary now.",
        ));
        let response = llm.chat(&history).await?;
        Ok(response)
    }
}

/// Build tool schemas filtered to only the tools allowed in a given phase.
fn phase_schemas(tools: &ToolRegistry, allowed: &[&str]) -> Vec<ToolSchema> {
    tools
        .tool_schemas()
        .into_iter()
        .filter(|s| allowed.contains(&s.name.as_str()))
        .collect()
}

/// Send a status update to the frontend (if a sender is available).
async fn send_status(tx: Option<&StatusSender>, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(CoreEvent::Status(msg.to_string())).await;
    }
}

/// Consume a streaming response into text + tool_calls + usage (same shape as non-streaming).
/// Forwards text deltas to the frontend.
async fn consume_stream(
    rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    status_tx: Option<&StatusSender>,
) -> (String, Vec<ToolCall>, Option<crate::llm::TokenUsage>) {
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

/// Try to extract a structured plan from the LLM response.
fn extract_plan(response: &str) -> Option<ExplorationResult> {
    let json = llm::extract_json(response)?;

    // Must have "plan" field and must NOT be a tool call
    if json.get("tool").is_some() {
        return None;
    }

    let plan = json.get("plan")?.as_str()?.to_string();
    if plan.is_empty() {
        return None;
    }

    Some(ExplorationResult {
        plan,
        context: json
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        files: json
            .get("files")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

// ── System prompts ──────────────────────────────────────────────────

fn build_explore_prompt_native(contract: &TaskContract) -> String {
    let soul_section = match &contract.soul {
        Some(soul) => format!("{}\n\n", soul),
        None => String::new(),
    };

    format!(
        r#"{soul}You are exploring a codebase to prepare for a coding task.

CONTEXT: {context}

INSTRUCTIONS:
- Use your tools to read files, search for patterns, and understand the architecture.
- When you have enough context, output a structured plan as a JSON object:
  {{"plan": "<step-by-step plan for the coding task>", "context": "<what you learned about the codebase>", "files": "<key file paths and relevant excerpts>"}}
- This plan will be passed to a coding agent that will execute it.
- Be thorough but efficient — you have at most {max_steps} exploration steps."#,
        soul = soul_section,
        context = contract.context,
        max_steps = MAX_EXPLORE_STEPS,
    )
}

fn build_explore_prompt(contract: &TaskContract, tools: &ToolRegistry) -> String {
    let soul_section = match &contract.soul {
        Some(soul) => format!("{}\n\n", soul),
        None => String::new(),
    };

    let tools_section = match &contract.tools_doc {
        Some(doc) => format!("\n\nTOOL REFERENCE:\n{}", doc),
        None => String::new(),
    };

    let tool_descs: String = EXPLORE_TOOLS
        .iter()
        .filter_map(|&name| tools.get(name))
        .map(|t| format!("- {}", t.description()))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"{soul}You are exploring a codebase to prepare for a coding task.

CONTEXT: {context}

AVAILABLE TOOLS (read-only):
{tools}{tools_doc}

INSTRUCTIONS:
- Read files, search for patterns, understand the architecture relevant to the task.
- To use a tool, respond with ONLY a JSON object: {{"tool": "<name>", "params": {{...}}}}
- Do NOT wrap JSON in markdown code blocks. Output raw JSON only when calling a tool.
- When you have enough context, output a structured plan as a JSON object:
  {{"plan": "<step-by-step plan for the coding task>", "context": "<what you learned about the codebase>", "files": "<key file paths and relevant excerpts>"}}
- This plan will be passed to a coding agent (Claude Code CLI) that will execute it.
- Be thorough but efficient — you have at most {max_steps} exploration steps."#,
        soul = soul_section,
        context = contract.context,
        tools = tool_descs,
        tools_doc = tools_section,
        max_steps = MAX_EXPLORE_STEPS,
    )
}

fn build_verify_prompt_native(contract: &TaskContract, exec_result: &str) -> String {
    let result_display = if exec_result.len() > 4000 {
        format!(
            "{}...\n[truncated, {} total chars]",
            &exec_result[..4000],
            exec_result.len()
        )
    } else {
        exec_result.to_string()
    };

    format!(
        r#"You are verifying the result of a coding task.

ORIGINAL GOAL: {goal}

CODING TOOL OUTPUT:
{result}

INSTRUCTIONS:
- Use your tools to read modified files and confirm changes are correct.
- Run compilation checks (e.g., cargo check) or tests if appropriate.
- When done verifying, respond with a plain-text summary of what was accomplished and any issues found.
- Be concise. Focus on correctness, not style."#,
        goal = contract.goal,
        result = result_display,
    )
}

fn build_verify_prompt(contract: &TaskContract, tools: &ToolRegistry, exec_result: &str) -> String {
    let tool_descs: String = VERIFY_TOOLS
        .iter()
        .filter_map(|&name| tools.get(name))
        .map(|t| format!("- {}", t.description()))
        .collect::<Vec<_>>()
        .join("\n");

    let result_display = if exec_result.len() > 4000 {
        format!(
            "{}...\n[truncated, {} total chars]",
            &exec_result[..4000],
            exec_result.len()
        )
    } else {
        exec_result.to_string()
    };

    format!(
        r#"You are verifying the result of a coding task.

ORIGINAL GOAL: {goal}

CODING TOOL OUTPUT:
{result}

AVAILABLE TOOLS:
{tools}

INSTRUCTIONS:
- To use a tool, respond with ONLY a JSON object: {{"tool": "<name>", "params": {{...}}}}
- Read modified files to confirm changes are correct.
- Run compilation checks (e.g., cargo check) or tests if appropriate.
- When done verifying, respond with a plain-text summary of what was accomplished and any issues found.
- Be concise. Focus on correctness, not style."#,
        goal = contract.goal,
        result = result_display,
        tools = tool_descs,
    )
}
