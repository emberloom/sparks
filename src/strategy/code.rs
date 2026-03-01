use async_trait::async_trait;
use tracing::Instrument;

use crate::confirm::Confirmer;
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::executor::Executor;
use crate::langfuse::ActiveTrace;
use crate::llm::{
    self, ChatMessage, ChatResponse, LlmProvider, Message, StreamEvent, TokenBudget, ToolCall,
    ToolSchema,
};
use crate::tools::ToolRegistry;

use super::{LoopStrategy, StatusSender, TaskContract};

/// Read-only tools allowed in the EXPLORE phase
const EXPLORE_TOOLS: &[&str] = &[
    "file_read",
    "grep",
    "glob",
    "codebase_map",
    "shell",
    "diff",
    "git",
    "gh",
];
/// Tools allowed in the VERIFY phase (read-only + lint)
const VERIFY_TOOLS: &[&str] = &[
    "file_read",
    "grep",
    "glob",
    "shell",
    "lint",
    "diff",
    "git",
    "gh",
];
/// Extended VERIFY tools when test generation is enabled (adds write + test_runner)
const VERIFY_WITH_TESTS_TOOLS: &[&str] = &[
    "file_read",
    "file_write",
    "file_edit",
    "grep",
    "glob",
    "shell",
    "lint",
    "test_runner",
    "diff",
    "git",
    "gh",
];
/// Coding CLI tools for the EXECUTE phase
const CODING_TOOLS: &[&str] = &["claude_code", "codex", "opencode"];

const MAX_EXPLORE_STEPS: usize = 5;
const MAX_VERIFY_STEPS: usize = 5;
const MAX_SELF_HEAL_ATTEMPTS: usize = 2;
const CLI_CONTRACT_PREFIX: &str = "[athena_cli_contract]";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliFailurePolicy {
    code: String,
    retry_same: bool,
    fallback: bool,
}

impl Default for CliFailurePolicy {
    fn default() -> Self {
        Self {
            code: "unclassified".to_string(),
            retry_same: false,
            fallback: true,
        }
    }
}

struct ExplorationResult {
    plan: String,
    context: String,
    files: String,
}

/// Build a ripple-effect warning from exploration file list.
fn build_ripple_section(files: &str) -> String {
    if files.is_empty() {
        return String::new();
    }
    let file_names: Vec<&str> = files
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_start_matches("- ");
            if trimmed.ends_with(".rs")
                || trimmed.ends_with(".py")
                || trimmed.ends_with(".ts")
                || trimmed.ends_with(".go")
                || trimmed.contains("src/")
            {
                Some(trimmed.split_whitespace().next().unwrap_or(trimmed))
            } else {
                None
            }
        })
        .take(10)
        .collect();
    if file_names.is_empty() {
        String::new()
    } else {
        format!(
            "\nRIPPLE WARNING: Changes to these files may affect other modules that import from them: {}. \
             Check for breaking changes to public APIs.",
            file_names.join(", ")
        )
    }
}

/// Build the full execution prompt from exploration results, contract, and ripple analysis.
fn build_execution_prompt(
    exploration: &ExplorationResult,
    contract: &TaskContract,
    ripple_section: &str,
) -> String {
    let mut parts = Vec::new();
    if !exploration.context.is_empty() {
        parts.push(format!("CODEBASE CONTEXT:\n{}", exploration.context));
    }
    parts.push(format!("TASK:\n{}", exploration.plan));
    if !contract.constraints.is_empty() {
        parts.push(format!(
            "CONSTRAINTS:\n{}",
            contract
                .constraints
                .iter()
                .map(|c| format!("- {}", c))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !ripple_section.is_empty() {
        parts.push(ripple_section.to_string());
    }
    parts.join("\n\n")
}

/// Attempt a prompt-rewrite retry when the coding tool fails on the last candidate.
async fn retry_with_prompt_rewrite(
    tool_name: &str,
    tool: &dyn crate::tools::Tool,
    docker: &DockerSession,
    llm: &dyn LlmProvider,
    full_prompt: &str,
    context: &str,
    files: &str,
    error_output: &str,
    failures: &mut Vec<String>,
) -> Result<Option<String>> {
    tracing::warn!(
        tool = tool_name,
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
            full_prompt, error_output
        )),
    ];

    let revised_prompt = llm.chat(&retry_messages).await?;
    let retry_params = serde_json::json!({
        "prompt": revised_prompt,
        "context": format!("{}\n\nPrevious attempt failed with:\n{}", context, error_output),
        "files": files,
    });

    tracing::info!(tool = tool_name, "EXECUTE: retrying with revised prompt");
    match tool.execute(docker, &retry_params).await {
        Ok(retry_result) if retry_result.success => Ok(Some(retry_result.output)),
        Ok(retry_result) => {
            failures.push(format!(
                "{} (retry): {}",
                tool_name,
                lf_truncate(&retry_result.output, 250)
            ));
            Ok(None)
        }
        Err(e) => {
            failures.push(format!("{} (retry): {}", tool_name, e));
            Ok(None)
        }
    }
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
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        let use_native = llm.supports_tools();
        let benchmark_fast = is_benchmark_fast_cli_mode(contract);

        // Phase 1: EXPLORE
        let exploration = if benchmark_fast {
            tracing::info!("CodeStrategy: benchmark fast mode enabled, skipping EXPLORE");
            send_status(status_tx, "Benchmark fast mode: skipping explore phase").await;
            ExplorationResult {
                plan: contract.goal.clone(),
                context: String::new(),
                files: String::new(),
            }
        } else {
            tracing::info!("CodeStrategy: starting EXPLORE phase");
            send_status(status_tx, "Exploring codebase...").await;
            let explore_span = trace.map(|t| t.span("phase:explore", None));
            let exploration = if use_native {
                self.explore_native(
                    contract, tools, docker, llm, executor, confirmer, status_tx, trace,
                )
                .instrument(tracing::info_span!("explore"))
                .await?
            } else {
                self.explore_text_fallback(
                    contract, tools, docker, llm, executor, confirmer, status_tx, trace,
                )
                .instrument(tracing::info_span!("explore"))
                .await?
            };
            if let Some(s) = explore_span {
                s.end(Some(&lf_truncate(&exploration.plan, 500)));
            }
            exploration
        };

        // Phase 2: EXECUTE (calls CLI tool directly)
        tracing::info!("CodeStrategy: starting EXECUTE phase");
        send_status(status_tx, "Executing code changes...").await;
        let exec_span =
            trace.map(|t| t.span("phase:execute", Some(&lf_truncate(&exploration.plan, 300))));
        let exec_result = self
            .execute_code(contract, tools, docker, llm, &exploration, !benchmark_fast)
            .instrument(tracing::info_span!("execute"))
            .await?;
        if let Some(s) = exec_span {
            s.end(Some(&lf_truncate(&exec_result, 500)));
        }

        if benchmark_fast {
            tracing::info!("CodeStrategy: benchmark fast mode enabled, skipping VERIFY");
            send_status(status_tx, "Benchmark fast mode: skipping verify phase").await;
            return Ok(exec_result);
        }

        // Phase 3: VERIFY
        tracing::info!("CodeStrategy: starting VERIFY phase");
        send_status(status_tx, "Verifying changes...").await;
        let verify_span = trace.map(|t| t.span("phase:verify", None));
        let summary = if use_native {
            self.verify_native(
                contract,
                tools,
                docker,
                llm,
                &exec_result,
                executor,
                confirmer,
                status_tx,
                trace,
            )
            .instrument(tracing::info_span!("verify"))
            .await?
        } else {
            self.verify_text_fallback(
                contract,
                tools,
                docker,
                llm,
                &exec_result,
                executor,
                confirmer,
                status_tx,
                trace,
            )
            .instrument(tracing::info_span!("verify"))
            .await?
        };
        if let Some(s) = verify_span {
            s.end(Some(&lf_truncate(&summary, 500)));
        }

        // Phase 3b: SELF-HEAL — if test failures detected, attempt corrective cycles
        if contract.test_generation {
            if let Some(fix_summary) = self
                .run_self_heal(
                    contract, tools, docker, llm, executor, confirmer, status_tx, trace,
                    use_native, &summary,
                )
                .await?
            {
                return Ok(fix_summary);
            }
        }

        Ok(summary)
    }
}

impl CodeStrategy {
    #[allow(clippy::too_many_arguments)]
    async fn run_self_heal(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        executor: &Executor,
        confirmer: &dyn Confirmer,
        status_tx: Option<&StatusSender>,
        trace: Option<&ActiveTrace>,
        use_native: bool,
        initial_summary: &str,
    ) -> Result<Option<String>> {
        let mut summary = initial_summary.to_string();
        for attempt in 0..MAX_SELF_HEAL_ATTEMPTS {
            let error_category =
                crate::self_heal::classify_test_failure_category(&summary).to_string();

            let prior_success_pattern = contract.memory.as_ref().and_then(|memory| {
                crate::self_heal::find_successful_fix_pattern(memory, &error_category)
            });

            let Some(mut fix_contract) =
                crate::self_heal::attempt_test_fix(&summary, &contract.goal)
            else {
                // No test failures detected. If a previous attempt already
                // applied a fix, return the current (now-passing) summary so
                // the caller uses the post-fix result instead of the original.
                if attempt > 0 {
                    return Ok(Some(summary));
                }
                return Ok(None);
            };

            if let Some(pattern) = prior_success_pattern {
                fix_contract.context = format!(
                    "{}\n\nRecent successful fix pattern for {}:\n{}",
                    fix_contract.context, error_category, pattern
                );
            }

            tracing::warn!(
                attempt = attempt + 1,
                max_attempts = MAX_SELF_HEAL_ATTEMPTS,
                "CodeStrategy: test failures detected, attempting self-heal"
            );
            send_status(status_tx, "Test failures detected — attempting fix...").await;
            let heal_span =
                trace.map(|t| t.span("phase:self_heal", Some(&lf_truncate(&summary, 300))));

            let fix_exploration = ExplorationResult {
                plan: fix_contract.goal.clone(),
                context: fix_contract.context.clone(),
                files: String::new(),
            };

            let fix_result = match self
                .execute_code(&fix_contract, tools, docker, llm, &fix_exploration, true)
                .instrument(tracing::info_span!("self_heal_execute"))
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!(error = %e, "CodeStrategy: self-heal EXECUTE failed");
                    if let Some(memory) = contract.memory.as_ref() {
                        crate::self_heal::store_self_heal_outcome(
                            memory,
                            &error_category,
                            &fix_contract.goal,
                            false,
                        );
                    }
                    if let Some(s) = heal_span {
                        s.end(Some(&format!("failed: {}", e)));
                    }
                    return Ok(None);
                }
            };

            send_status(status_tx, "Re-verifying after fix...").await;
            summary = if use_native {
                self.verify_native(
                    contract,
                    tools,
                    docker,
                    llm,
                    &fix_result,
                    executor,
                    confirmer,
                    status_tx,
                    trace,
                )
                .instrument(tracing::info_span!("self_heal_verify"))
                .await?
            } else {
                self.verify_text_fallback(
                    contract,
                    tools,
                    docker,
                    llm,
                    &fix_result,
                    executor,
                    confirmer,
                    status_tx,
                    trace,
                )
                .instrument(tracing::info_span!("self_heal_verify"))
                .await?
            };

            let success = !crate::self_heal::has_test_failures(&summary);
            if let Some(memory) = contract.memory.as_ref() {
                crate::self_heal::store_self_heal_outcome(
                    memory,
                    &error_category,
                    &fix_contract.goal,
                    success,
                );
            }
            tracing::info!("CodeStrategy: self-heal cycle complete");
            if let Some(s) = heal_span {
                s.end(Some("fixed"));
            }

            if attempt + 1 >= MAX_SELF_HEAL_ATTEMPTS {
                tracing::warn!("CodeStrategy: self-heal attempts exhausted");
                return Ok(Some(summary));
            }
        }

        Ok(None)
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
        trace: Option<&ActiveTrace>,
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
                        tracing::info!(
                            step,
                            path = "native",
                            "EXPLORE complete — got plan from text"
                        );
                        return Ok(plan);
                    }
                }

                let text = if text_accum.is_empty() {
                    None
                } else {
                    Some(text_accum)
                };
                history.push(ChatMessage::Assistant {
                    content: text,
                    tool_calls: Some(tool_calls.clone()),
                });

                // Split into allowed and disallowed tool calls
                let (allowed, disallowed): (Vec<_>, Vec<_>) = tool_calls
                    .iter()
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
                let futs: Vec<_> = allowed
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
                        (*tc, result)
                    })
                    .collect();

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
             {\"plan\": \"...\", \"context\": \"...\", \"files\": \"...\"}"
                .to_string(),
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
        trace: Option<&ActiveTrace>,
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
                .execute_tool(tool_name, &json, tools, docker, confirmer, status_tx, trace)
                .await?;

            tracing::debug!(
                step,
                tool = tool_name,
                path = "text",
                "EXPLORE tool executed"
            );
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
    /// Includes optional ripple effect analysis from code_structure memories.
    async fn execute_code(
        &self,
        contract: &TaskContract,
        tools: &ToolRegistry,
        docker: &DockerSession,
        llm: &dyn LlmProvider,
        exploration: &ExplorationResult,
        allow_retry_rewrite: bool,
    ) -> Result<String> {
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(pref) = contract
            .cli_tool_preference
            .as_deref()
            .filter(|pref| tools.get(pref).is_some())
        {
            candidates.push(pref);
        }
        for &name in CODING_TOOLS {
            if tools.get(name).is_some() && !candidates.contains(&name) {
                candidates.push(name);
            }
        }
        if candidates.is_empty() {
            return Err(AthenaError::Tool(
                "No coding CLI tool available (need claude_code, codex, or opencode)".into(),
            ));
        }

        let ripple_section = build_ripple_section(&exploration.files);

        let full_prompt = build_execution_prompt(exploration, contract, &ripple_section);
        let mut failures: Vec<String> = Vec::new();

        for (idx, &tool_name) in candidates.iter().enumerate() {
            if tool_name == "claude_code" && std::env::var_os("CLAUDECODE").is_some() {
                let msg = "Skipping claude_code: running inside a Claude Code session";
                tracing::warn!("{}", msg);
                failures.push(format!("{}: {}", tool_name, msg));
                continue;
            }

            let tool = match tools.get(tool_name) {
                Some(t) => t,
                None => {
                    failures.push(format!("{}: not available", tool_name));
                    continue;
                }
            };

            let params = serde_json::json!({
                "prompt": full_prompt,
                "context": contract.context,
                "files": exploration.files,
            });

            tracing::info!(tool = tool_name, "EXECUTE: calling coding tool");
            let result = match tool.execute(docker, &params).await {
                Ok(r) => r,
                Err(e) => {
                    failures.push(format!("{}: {}", tool_name, e));
                    continue;
                }
            };

            if result.success {
                tracing::info!(tool = tool_name, "EXECUTE: coding tool succeeded");
                return Ok(result.output);
            }
            let policy = parse_cli_failure_policy(&result.output);

            failures.push(format!(
                "{} [{}]: {}",
                tool_name,
                policy.code,
                lf_truncate(&result.output, 250)
            ));

            if policy.retry_same {
                tracing::warn!(
                    tool = tool_name,
                    code = %policy.code,
                    "EXECUTE: retrying same coding tool based on policy"
                );
                match tool.execute(docker, &params).await {
                    Ok(retry_once) if retry_once.success => return Ok(retry_once.output),
                    Ok(retry_once) => {
                        let retry_policy = parse_cli_failure_policy(&retry_once.output);
                        failures.push(format!(
                            "{} (policy-retry) [{}]: {}",
                            tool_name,
                            retry_policy.code,
                            lf_truncate(&retry_once.output, 250)
                        ));
                        if !retry_policy.fallback {
                            return Err(AthenaError::Tool(format!(
                                "Coding tool '{}' failed with non-fallback policy code '{}'.\n{}",
                                tool_name,
                                retry_policy.code,
                                failures
                                    .iter()
                                    .map(|f| format!("- {}", f))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )));
                        }
                    }
                    Err(e) => failures.push(format!("{} (policy-retry): {}", tool_name, e)),
                }
            }

            if !policy.fallback {
                return Err(AthenaError::Tool(format!(
                    "Coding tool '{}' failed with non-fallback policy code '{}'.\n{}",
                    tool_name,
                    policy.code,
                    failures
                        .iter()
                        .map(|f| format!("- {}", f))
                        .collect::<Vec<_>>()
                        .join("\n")
                )));
            }

            if allow_retry_rewrite && policy.fallback && idx + 1 == candidates.len() {
                if let Some(output) = retry_with_prompt_rewrite(
                    tool_name,
                    tool,
                    docker,
                    llm,
                    &full_prompt,
                    &contract.context,
                    &exploration.files,
                    &result.output,
                    &mut failures,
                )
                .await?
                {
                    return Ok(output);
                }
            }
        }

        Err(AthenaError::Tool(format!(
            "All coding CLI tools failed.\n{}",
            failures
                .iter()
                .map(|f| format!("- {}", f))
                .collect::<Vec<_>>()
                .join("\n")
        )))
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
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        let system_prompt = build_verify_prompt_native(contract, exec_result);
        let verify_tools = if contract.test_generation {
            VERIFY_WITH_TESTS_TOOLS
        } else {
            VERIFY_TOOLS
        };
        let schemas = phase_schemas(tools, verify_tools);

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
                let text = if text_accum.is_empty() {
                    None
                } else {
                    Some(text_accum)
                };
                history.push(ChatMessage::Assistant {
                    content: text,
                    tool_calls: Some(tool_calls.clone()),
                });

                // Split into allowed and disallowed tool calls
                let (allowed, disallowed): (Vec<_>, Vec<_>) = tool_calls
                    .iter()
                    .partition(|tc| verify_tools.contains(&tc.name.as_str()));

                for tc in &disallowed {
                    history.push(ChatMessage::Tool {
                        tool_call_id: tc.id.clone(),
                        content: format!(
                            "Tool '{}' is not allowed in the verification phase. Use only: {}",
                            tc.name,
                            verify_tools.join(", ")
                        ),
                    });
                }

                // Execute allowed tool calls in parallel
                for tc in &allowed {
                    send_status(status_tx, &format!("Verifying: {} ...", tc.name)).await;
                }
                let futs: Vec<_> = allowed
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
                        (*tc, result)
                    })
                    .collect();

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
        trace: Option<&ActiveTrace>,
    ) -> Result<String> {
        let verify_tools = if contract.test_generation {
            VERIFY_WITH_TESTS_TOOLS
        } else {
            VERIFY_TOOLS
        };
        let system_prompt = build_verify_prompt(contract, tools, exec_result, verify_tools);
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

            if !verify_tools.contains(&tool_name) {
                history.push(Message::user(&format!(
                    "Tool '{}' is not allowed in the verification phase. Use only: {}",
                    tool_name,
                    verify_tools.join(", ")
                )));
                continue;
            }

            send_status(status_tx, &format!("Verifying: {} ...", tool_name)).await;

            let tool_output = executor
                .execute_tool(tool_name, &json, tools, docker, confirmer, status_tx, trace)
                .await?;

            tracing::debug!(
                step,
                tool = tool_name,
                path = "text",
                "VERIFY tool executed"
            );
            history.push(Message::user(&tool_output));
        }

        history.push(Message::user(
            "Verification step limit reached. Provide your final summary now.",
        ));
        let response = llm.chat(&history).await?;
        Ok(response)
    }
}

fn is_benchmark_fast_cli_mode(contract: &TaskContract) -> bool {
    let context = contract.context.to_lowercase();
    context.contains("[benchmark_fast_cli]") || context.contains("[eval_fast_cli]")
}

fn parse_cli_failure_policy(output: &str) -> CliFailurePolicy {
    let mut policy = CliFailurePolicy::default();
    let Some(line) = output
        .lines()
        .find(|line| line.contains(CLI_CONTRACT_PREFIX))
    else {
        return policy;
    };
    let Some(start) = line.find(CLI_CONTRACT_PREFIX) else {
        return policy;
    };
    let contract = &line[start..];
    for token in contract.split_whitespace().skip(1) {
        let Some((k, v)) = token.split_once('=') else {
            continue;
        };
        match k {
            "code" => {
                if !v.is_empty() {
                    policy.code = v.to_string();
                }
            }
            "retry_same" => {
                if let Some(parsed) = parse_cli_contract_bool(v) {
                    policy.retry_same = parsed;
                }
            }
            "fallback" => {
                if let Some(parsed) = parse_cli_contract_bool(v) {
                    policy.fallback = parsed;
                }
            }
            _ => {}
        }
    }
    policy
}

fn parse_cli_contract_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
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
- This plan will be passed to a coding CLI agent that will execute it.
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

    let test_gen_section = if contract.test_generation {
        r#"
- IMPORTANT: You have write access in this verification phase.
- After reading the diff of changes, write focused #[test] functions that cover the new/modified behavior.
- Place tests in the appropriate test module (e.g., `#[cfg(test)] mod tests` block in the same file).
- Limit test generation to at most 3 test files and 500 lines total.
- Run the tests with `test_runner` to confirm they pass.
- If tests fail, fix the IMPLEMENTATION (not the tests) and re-run."#
    } else {
        ""
    };

    format!(
        r#"You are verifying the result of a coding task.

ORIGINAL GOAL: {goal}

CODING TOOL OUTPUT:
{result}

INSTRUCTIONS:
- Use your tools to read modified files and confirm changes are correct.
- Run compilation checks (e.g., cargo check) or tests if appropriate.{test_gen}
- When done verifying, respond with a plain-text summary of what was accomplished and any issues found.
- Be concise. Focus on correctness, not style."#,
        goal = contract.goal,
        result = result_display,
        test_gen = test_gen_section,
    )
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

fn build_verify_prompt(
    contract: &TaskContract,
    tools: &ToolRegistry,
    exec_result: &str,
    verify_tools: &[&str],
) -> String {
    let tool_descs: String = verify_tools
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

    let test_gen_section = if contract.test_generation {
        r#"
- IMPORTANT: You have write access in this verification phase.
- After reviewing the diff, add focused #[test] cases and run them with `test_runner`.
- If tests fail, fix the implementation (not the tests) and re-run."#
    } else {
        ""
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
- Run compilation checks (e.g., cargo check) or tests if appropriate.{test_gen}
- When done verifying, respond with a plain-text summary of what was accomplished and any issues found.
- Be concise. Focus on correctness, not style."#,
        goal = contract.goal,
        result = result_display,
        tools = tool_descs,
        test_gen = test_gen_section,
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_cli_failure_policy, CliFailurePolicy};

    #[test]
    fn parse_cli_failure_policy_defaults_for_plain_text() {
        let policy = parse_cli_failure_policy("plain failure text");
        assert_eq!(
            policy,
            CliFailurePolicy {
                code: "unclassified".to_string(),
                retry_same: false,
                fallback: true,
            }
        );
    }

    #[test]
    fn parse_cli_failure_policy_reads_contract_fields_marker_first_line() {
        let policy = parse_cli_failure_policy(
            "[athena_cli_contract] tool=codex code=transient_upstream retry_same=true fallback=true\nstderr noise",
        );
        assert_eq!(policy.code, "transient_upstream");
        assert!(policy.retry_same);
        assert!(policy.fallback);
    }

    #[test]
    fn parse_cli_failure_policy_reads_contract_fields_marker_middle_line() {
        let policy = parse_cli_failure_policy(
            "warning: something\nstderr: [athena_cli_contract] tool=codex code=rate_limit retry_same=false fallback=false\nmore noise",
        );
        assert_eq!(policy.code, "rate_limit");
        assert!(!policy.retry_same);
        assert!(!policy.fallback);
    }

    #[test]
    fn parse_cli_failure_policy_ignores_malformed_tokens() {
        let policy = parse_cli_failure_policy(
            "[athena_cli_contract] tool=codex code=invalid_request retry_same=maybe fallback=TRUEE",
        );
        assert_eq!(policy.code, "invalid_request");
        assert!(!policy.retry_same);
        assert!(policy.fallback);
    }

    #[test]
    fn parse_cli_failure_policy_non_fallback_is_deterministic() {
        let a = parse_cli_failure_policy(
            "[athena_cli_contract] tool=codex code=invalid_request retry_same=false fallback=false",
        );
        let b = parse_cli_failure_policy(
            "[athena_cli_contract] tool=codex code=invalid_request retry_same=false fallback=false",
        );
        assert_eq!(a, b);
        assert_eq!(a.code, "invalid_request");
        assert!(!a.retry_same);
        assert!(!a.fallback);
    }
}
