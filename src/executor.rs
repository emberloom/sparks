use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{DockerConfig, GhostConfig, LoopGuardConfig};
use crate::confirm::{Confirmer, SensitivePatterns};
use crate::core::CoreEvent;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::llm::LlmProvider;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::reason_codes::{self, REASON_LOOP_GUARD_TRIGGERED};
use crate::self_heal;
use crate::strategy::{self, StatusSender, TaskContract};
use crate::tool_usage::ToolUsageStore;
use crate::tools::ToolRegistry;

#[derive(Debug, Clone)]
struct ToolLoopGuard {
    enabled: bool,
    window_size: usize,
    repeat_threshold: usize,
    sessions: Arc<Mutex<HashMap<String, SessionLoopState>>>,
}

#[derive(Debug, Clone)]
struct LoopObservation {
    fingerprint: String,
    repeats: usize,
    triggered: bool,
}

#[derive(Debug, Default)]
struct SessionLoopState {
    recent: VecDeque<String>,
    counts: HashMap<String, usize>,
}

impl ToolLoopGuard {
    fn new(config: &LoopGuardConfig) -> Self {
        Self {
            enabled: config.enabled,
            window_size: config.window_size.max(1),
            repeat_threshold: config.repeat_threshold.max(1),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn observe(
        &self,
        session_id: &str,
        tool_name: &str,
        params: &Value,
    ) -> Option<LoopObservation> {
        if !self.enabled {
            return None;
        }
        let fingerprint = tool_call_fingerprint(tool_name, params);
        let mut sessions = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!(
                    "Loop guard state mutex was poisoned; continuing with recovered state"
                );
                poisoned.into_inner()
            }
        };
        let state = sessions.entry(session_id.to_string()).or_default();
        state.recent.push_back(fingerprint.clone());
        *state.counts.entry(fingerprint.clone()).or_insert(0) += 1;
        while state.recent.len() > self.window_size {
            if let Some(old) = state.recent.pop_front() {
                if let Some(count) = state.counts.get_mut(&old) {
                    *count -= 1;
                    if *count == 0 {
                        state.counts.remove(&old);
                    }
                }
            }
        }
        let repeats = state.counts.get(&fingerprint).copied().unwrap_or(0);
        Some(LoopObservation {
            fingerprint,
            repeats,
            triggered: repeats >= self.repeat_threshold,
        })
    }

    fn clear_session(&self, session_id: &str) {
        let mut sessions = match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("Loop guard state mutex was poisoned during cleanup; continuing");
                poisoned.into_inner()
            }
        };
        sessions.remove(session_id);
    }
}

fn normalize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut ordered = BTreeMap::new();
            for (k, v) in map {
                ordered.insert(k.clone(), normalize_value(v));
            }
            Value::Object(ordered.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_value).collect()),
        _ => value.clone(),
    }
}

fn tool_call_fingerprint(tool_name: &str, params: &Value) -> String {
    let normalized = normalize_value(params);
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\n");
    hasher.update(serde_json::to_vec(&normalized).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

pub struct Executor {
    docker_config: DockerConfig,
    self_dev_trusted_mode: bool,
    trusted_repos: Vec<String>,
    max_steps: usize,
    sensitive_patterns: SensitivePatterns,
    dynamic_tools_path: Option<PathBuf>,
    knobs: SharedKnobs,
    github_token: Option<String>,
    usage_store: Arc<ToolUsageStore>,
    observer: ObserverHandle,
    loop_guard: ToolLoopGuard,
    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    langfuse: SharedLangfuse,
}

impl Executor {
    pub fn new(
        docker_config: DockerConfig,
        self_dev_trusted_mode: bool,
        trusted_repos: Vec<String>,
        max_steps: usize,
        sensitive_patterns: Vec<String>,
        loop_guard_config: LoopGuardConfig,
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
            self_dev_trusted_mode,
            trusted_repos,
            max_steps,
            sensitive_patterns: compiled,
            dynamic_tools_path,
            knobs,
            github_token,
            usage_store,
            observer,
            loop_guard: ToolLoopGuard::new(&loop_guard_config),
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
        let trusted_repo_policy = if self.self_dev_trusted_mode {
            Some(self.trusted_repos.as_slice())
        } else {
            None
        };
        let session = DockerSession::new(ghost, &self.docker_config, trusted_repo_policy).await?;
        let tools = ToolRegistry::for_ghost(
            ghost,
            self.dynamic_tools_path.as_deref(),
            self.knobs.clone(),
            self.github_token.clone(),
            Some(self.usage_store.clone()),
        );
        let strategy = strategy::strategy_from_config(&ghost.strategy)?;

        // Try direct tool completion first (precheck)
        let precheck = match strategy::try_direct_completion(
            contract, &tools, &session, llm, self, confirmer, status_tx, trace,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                self.close_session(session).await;
                return Err(e);
            }
        };
        if let Some(result) = precheck {
            tracing::info!(ghost = %ghost.name, "Task completed via direct tool use (precheck)");
            self.close_session(session).await;
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
        self.close_session(session).await;

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

    async fn close_session(&self, session: DockerSession) {
        self.loop_guard.clear_session(session.session_id());
        if let Err(e) = session.close().await {
            tracing::warn!("Failed to close container: {}", e);
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

        if let Some(loop_obs) = self
            .loop_guard
            .observe(docker.session_id(), tool_name, &params)
            .filter(|obs| obs.triggered)
        {
            let short_fp = &loop_obs.fingerprint[..12];
            let loop_message = reason_codes::with_reason(
                REASON_LOOP_GUARD_TRIGGERED,
                format!(
                    "Loop guard blocked repeated tool call '{}' (repeats={} window={}). Change arguments or choose a different tool before retrying. fingerprint={}",
                    tool_name,
                    loop_obs.repeats,
                    self.loop_guard.window_size,
                    short_fp
                ),
            );
            self.observer.log(
                ObserverCategory::ToolUsage,
                format!(
                    "loop_guard tool={} repeats={} window={} fingerprint={}",
                    tool_name, loop_obs.repeats, self.loop_guard.window_size, short_fp
                ),
            );
            if let Err(e) = self
                .usage_store
                .record("loop_guard", false, 0.0, Some(&loop_message))
            {
                tracing::warn!("Failed to record loop guard usage: {}", e);
            }
            return Err(AthenaError::Tool(loop_message));
        }

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

#[cfg(test)]
mod tests {
    use super::ToolLoopGuard;
    use crate::config::LoopGuardConfig;
    use serde_json::json;

    #[test]
    fn loop_guard_triggers_on_repeated_identical_call() {
        let guard = ToolLoopGuard::new(&LoopGuardConfig {
            enabled: true,
            window_size: 8,
            repeat_threshold: 2,
        });
        let first = guard
            .observe("session-a", "shell", &json!({ "command": "cargo check" }))
            .expect("loop guard should be enabled");
        assert!(!first.triggered);
        assert_eq!(first.repeats, 1);

        let second = guard
            .observe("session-a", "shell", &json!({ "command": "cargo check" }))
            .expect("loop guard should be enabled");
        assert!(second.triggered);
        assert_eq!(second.repeats, 2);
    }

    #[test]
    fn loop_guard_allows_changed_arguments() {
        let guard = ToolLoopGuard::new(&LoopGuardConfig {
            enabled: true,
            window_size: 8,
            repeat_threshold: 2,
        });
        let _ = guard.observe("session-b", "grep", &json!({ "pattern": "foo" }));
        let changed = guard
            .observe("session-b", "grep", &json!({ "pattern": "bar" }))
            .expect("loop guard should be enabled");
        assert!(!changed.triggered);
        assert_eq!(changed.repeats, 1);
    }

    #[test]
    fn loop_guard_is_bounded_by_window() {
        let guard = ToolLoopGuard::new(&LoopGuardConfig {
            enabled: true,
            window_size: 2,
            repeat_threshold: 2,
        });
        let _ = guard.observe("session-c", "shell", &json!({ "command": "A" }));
        let _ = guard.observe("session-c", "shell", &json!({ "command": "B" }));
        let third = guard
            .observe("session-c", "shell", &json!({ "command": "A" }))
            .expect("loop guard should be enabled");
        assert!(!third.triggered);
        assert_eq!(third.repeats, 1);
    }

    #[test]
    fn loop_guard_normalizes_param_key_order() {
        let guard = ToolLoopGuard::new(&LoopGuardConfig {
            enabled: true,
            window_size: 8,
            repeat_threshold: 2,
        });
        let _ = guard.observe(
            "session-d",
            "shell",
            &json!({ "command": "cargo test", "cwd": "/workspace" }),
        );
        let second = guard
            .observe(
                "session-d",
                "shell",
                &json!({ "cwd": "/workspace", "command": "cargo test" }),
            )
            .expect("loop guard should be enabled");
        assert!(second.triggered);
    }

    #[test]
    fn loop_guard_can_be_disabled() {
        let guard = ToolLoopGuard::new(&LoopGuardConfig {
            enabled: false,
            window_size: 8,
            repeat_threshold: 2,
        });
        assert!(guard
            .observe("session-e", "shell", &json!({ "command": "cargo check" }))
            .is_none());
    }
}
