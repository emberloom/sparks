use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::llm::{LlmProvider, Message};
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::{Pulse, PulseBus, PulseSource, Urgency};
use crate::randomness;
use crate::scheduler::Schedule;

/// Tracks the timestamp of the last user interaction (epoch seconds).
pub struct ActivityTracker {
    last_activity: AtomicU64,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self {
            last_activity: AtomicU64::new(now_secs()),
        }
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    pub fn idle_secs(&self) -> u64 {
        now_secs().saturating_sub(self.last_activity.load(Ordering::Relaxed))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn build_recent_memory_lines(memories: &[crate::memory::Memory], limit: usize) -> Vec<String> {
    memories
        .iter()
        .take(limit)
        .map(|m| format!("- [{}] {}", m.category, m.content))
        .collect()
}

fn build_memory_scan_prompt(recent: &[String]) -> String {
    format!(
        r#"Review these recent memories and identify any interesting patterns, connections, or insights:

{}

If you notice a meaningful pattern worth sharing, describe it in 1-2 sentences. If nothing stands out, respond with exactly: NO_PATTERN"#,
        recent.join("\n")
    )
}

fn has_similar_failure(idea: &str, past_failures: &[crate::memory::Memory]) -> bool {
    let lower_idea = idea.to_lowercase();
    past_failures.iter().any(|m| {
        let failure_lower = m.content.to_lowercase();
        lower_idea
            .split_whitespace()
            .filter(|w| w.len() > 5)
            .any(|word| failure_lower.contains(word))
    })
}

/// Shared helper: classify whether content implies a code improvement, gate on
/// spontaneity and past-failure similarity, and optionally dispatch a scout task.
async fn classify_and_dispatch_improvement(
    source_label: &str,
    content: &str,
    threshold: f32,
    spontaneity: f32,
    observer: &ObserverHandle,
    suppress_category: ObserverCategory,
    llm: &dyn LlmProvider,
    memory: &MemoryStore,
    auto_tx: &tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    lf_trace: &Option<ActiveTrace>,
    model_name: &str,
) {
    let classify_prompt = format!(
        "Does this {src} suggest a concrete code improvement? \
         {src}: \"{content}\"\n\n\
         If yes, describe the improvement in 1-2 sentences. \
         If no, respond NO_ACTION.",
        src = source_label,
        content = content,
    );
    let classify_gen = lf_trace
        .as_ref()
        .map(|t| t.generation("classify_improvement", model_name, None));
    let classify_msgs = vec![Message::user(&classify_prompt)];
    let Ok(classify_resp) = llm.chat(&classify_msgs).await else {
        return;
    };
    if let Some(g) = classify_gen {
        g.end(Some(&truncate(classify_resp.trim(), 500)), 0, 0);
    }
    let cr = classify_resp.trim();
    if cr.to_uppercase().contains("NO_ACTION") || is_refusal(cr) {
        return;
    }
    if !randomness::should_speak(threshold, spontaneity) {
        observer.log(
            ObserverCategory::StochasticRoll,
            format!(
                "{} improvement suppressed by gate (spontaneity={:.2})",
                source_label, spontaneity
            ),
        );
        return;
    }
    let past_failures = memory
        .search_hybrid("improvement_idea failed", None, 5)
        .unwrap_or_default();
    if has_similar_failure(cr, &past_failures) {
        observer.log(
            suppress_category,
            format!(
                "{} improvement suppressed: similar past failure found",
                source_label
            ),
        );
        return;
    }
    let _ = memory.store("improvement_idea", cr, None);
    let task = crate::core::AutonomousTask {
        goal: format!(
            "Investigate this improvement idea: {}\n\n\
             Explore feasibility, identify affected files, and report findings.",
            cr
        ),
        context: format!(
            "Discovered via {}. Investigation only — do not make code changes.",
            source_label
        ),
        ghost: Some("scout".to_string()),
        target: crate::pulse::PulseTarget::Broadcast,
        lane: "self_improvement".to_string(),
        risk_tier: "medium".to_string(),
        repo: crate::kpi::default_repo_name(),
        task_id: None,
    };
    if let Err(e) = auto_tx.send(task).await {
        tracing::warn!("{}: failed to dispatch improvement task: {}", source_label, e);
    }
}

/// Check for past failures, apply spontaneity gate, and dispatch a refactoring task.
async fn check_and_dispatch_refactoring(
    opportunity: &str,
    spontaneity: f32,
    observer: &ObserverHandle,
    memory: &MemoryStore,
    auto_tx: &tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    lf_trace: Option<ActiveTrace>,
) {
    let past_failures = memory
        .search_hybrid("refactoring_failed", None, 5)
        .unwrap_or_default();
    let similar_failure = has_similar_failure(opportunity, &past_failures);

    let suppress_span = lf_trace.as_ref().map(|t| t.span("failure_check", None));

    if similar_failure {
        observer.log(
            ObserverCategory::AutonomousTask,
            "Refactoring suppressed: similar past failure found",
        );
        if let Some(s) = suppress_span {
            s.end(Some("suppressed"));
        }
        if let Some(t) = lf_trace {
            t.end(Some("suppressed: similar past failure"));
        }
    } else if randomness::should_speak(0.3, spontaneity) {
        if let Some(s) = suppress_span {
            s.end(Some("passed"));
        }
        let dispatch_span = lf_trace
            .as_ref()
            .map(|t| t.span("dispatch_coder", Some(opportunity)));
        let task = crate::core::AutonomousTask {
            goal: format!(
                "Implement this refactoring: {}\n\n\
                 Be careful, run tests after changes, and keep the refactoring minimal.",
                opportunity
            ),
            context: "This is a suggested refactoring from automated analysis. \
                      Proceed carefully and verify with tests."
                .to_string(),
            ghost: Some("coder".to_string()),
            target: crate::pulse::PulseTarget::Broadcast,
            lane: "self_improvement".to_string(),
            risk_tier: "high".to_string(),
            repo: crate::kpi::default_repo_name(),
            task_id: None,
        };
        if let Err(e) = auto_tx.send(task).await {
            tracing::warn!("Refactoring scanner: failed to dispatch: {}", e);
        }
        if let Some(s) = dispatch_span {
            s.end(Some("task dispatched"));
        }
        if let Some(t) = lf_trace {
            t.end(Some("dispatched"));
        }
    } else {
        if let Some(s) = suppress_span {
            s.end(Some("gate_suppressed"));
        }
        if let Some(t) = lf_trace {
            t.end(Some("gate_suppressed"));
        }
    }
}

/// Spawn the memory pattern scanner loop.
pub fn spawn_memory_scanner(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    tokio::spawn(async move {
        loop {
            let (interval, spontaneity, enabled, all) = {
                let k = knobs.read().unwrap();
                (
                    k.memory_scan_interval_secs,
                    k.spontaneity,
                    k.memory_scan_enabled,
                    k.all_proactive,
                )
            };

            if !all || !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let sleep_dur = randomness::jitter_interval(interval, 0.3);
            tokio::time::sleep(sleep_dur).await;

            // Re-check knobs
            {
                let k = knobs.read().unwrap();
                if !k.all_proactive || !k.memory_scan_enabled {
                    continue;
                }
            }

            observer.log(ObserverCategory::MemoryScan, "Starting memory pattern scan");

            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "funnel3:memory_scan",
                    None,
                    None,
                    None,
                    vec!["funnel3", "memory_scan"],
                )
            });

            // Load recent memories
            let memories = match memory.list() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Memory scanner: failed to list: {}", e);
                    if let Some(t) = lf_trace {
                        t.end(Some("failed to list memories"));
                    }
                    continue;
                }
            };

            if memories.is_empty() {
                if let Some(t) = lf_trace {
                    t.end(Some("no memories"));
                }
                continue;
            }

            let recent = build_recent_memory_lines(&memories, 50);
            let prompt = build_memory_scan_prompt(&recent);

            let model_name = llm.provider_name().to_string();
            let gen = lf_trace
                .as_ref()
                .map(|t| t.generation("find_patterns", &model_name, None));

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim();
                    if let Some(g) = gen {
                        g.end(Some(&truncate(trimmed, 500)), 0, 0);
                    }

                    if is_refusal(trimmed) {
                        observer.log(ObserverCategory::MemoryScan, "No patterns found");
                        if let Some(t) = lf_trace {
                            t.end(Some("no_pattern"));
                        }
                        continue;
                    }

                    // Always store pattern as memory (enriches future scans)
                    let _ = memory.store("pattern", trimmed, None);

                    classify_and_dispatch_improvement(
                        "pattern", trimmed, 0.2, spontaneity,
                        &observer, ObserverCategory::MemoryScan,
                        &*llm, &memory, &auto_tx, &lf_trace, &model_name,
                    ).await;

                    // Stochastic gate for pulse delivery
                    if randomness::should_speak(0.6, spontaneity) {
                        let pulse =
                            Pulse::new(PulseSource::MemoryScan, Urgency::Low, trimmed.to_string());
                        pulse_bus.send(pulse);
                    } else {
                        observer.log(
                            ObserverCategory::StochasticRoll,
                            format!(
                                "Memory pattern suppressed by gate (spontaneity={:.2})",
                                spontaneity
                            ),
                        );
                    }

                    if let Some(t) = lf_trace {
                        t.end(Some(&truncate(trimmed, 200)));
                    }
                }
                Err(e) => {
                    tracing::warn!("Memory scanner LLM call failed: {}", e);
                    if let Some(g) = gen {
                        g.end(Some(&format!("error: {}", e)), 0, 0);
                    }
                    if let Some(t) = lf_trace {
                        t.end(Some(&format!("error: {}", e)));
                    }
                }
            }
        }
    });
}

/// Spawn the idle musings loop.
pub fn spawn_idle_musings(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    activity: Arc<ActivityTracker>,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    tokio::spawn(async move {
        loop {
            // Check every ~5 min
            tokio::time::sleep(randomness::jitter_interval(300, 0.3)).await;

            let (threshold, spontaneity, enabled, all) = {
                let k = knobs.read().unwrap();
                (
                    k.idle_threshold_secs,
                    k.spontaneity,
                    k.idle_musings_enabled,
                    k.all_proactive,
                )
            };

            if !all || !enabled {
                continue;
            }

            let idle = activity.idle_secs();
            if idle < threshold {
                continue;
            }

            observer.log(
                ObserverCategory::IdleMusing,
                format!("Idle for {}s, generating musing", idle),
            );

            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "funnel3:idle_musing",
                    None,
                    None,
                    None,
                    vec!["funnel3", "idle_musing"],
                )
            });

            // Sample random memories for reflection
            let memories = match memory.list() {
                Ok(m) => m,
                Err(_) => {
                    if let Some(t) = lf_trace {
                        t.end(Some("no memories"));
                    }
                    continue;
                }
            };

            if memories.is_empty() {
                if let Some(t) = lf_trace {
                    t.end(Some("no memories"));
                }
                continue;
            }

            let count = 5.min(memories.len());
            let indices = randomness::sample_indices(count, memories.len());
            let sampled: Vec<String> = indices
                .into_iter()
                .map(|i| format!("- [{}] {}", memories[i].category, memories[i].content))
                .collect();

            let prompt = format!(
                r#"You're in a quiet moment, reflecting on these memories:

{}

Synthesize a brief reflection or musing (1-2 sentences). Be thoughtful and natural."#,
                sampled.join("\n")
            );

            let model_name = llm.provider_name().to_string();
            let gen = lf_trace
                .as_ref()
                .map(|t| t.generation("muse", &model_name, None));

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim().to_string();
                    if let Some(g) = gen {
                        g.end(Some(&truncate(&trimmed, 500)), 0, 0);
                    }
                    observer.log(
                        ObserverCategory::IdleMusing,
                        format!("Generated musing: \"{}\"", truncate(&trimmed, 60)),
                    );

                    // Always store as memory
                    let _ = memory.store("musing", &trimmed, None);

                    classify_and_dispatch_improvement(
                        "musing", &trimmed, 0.15, spontaneity,
                        &observer, ObserverCategory::IdleMusing,
                        &*llm, &memory, &auto_tx, &lf_trace, &model_name,
                    ).await;

                    // Stochastic gate for pulse delivery
                    if randomness::should_speak(0.5, spontaneity) {
                        let pulse = Pulse::new(PulseSource::IdleMusing, Urgency::Low, trimmed);
                        pulse_bus.send(pulse);
                        if let Some(t) = lf_trace {
                            t.end(Some("pulse_emitted"));
                        }
                    } else {
                        observer.log(
                            ObserverCategory::StochasticRoll,
                            format!(
                                "Idle musing suppressed by gate (spontaneity={:.2})",
                                spontaneity
                            ),
                        );
                        if let Some(t) = lf_trace {
                            t.end(Some("suppressed"));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Idle musing LLM call failed: {}", e);
                    if let Some(g) = gen {
                        g.end(Some(&format!("error: {}", e)), 0, 0);
                    }
                    if let Some(t) = lf_trace {
                        t.end(Some(&format!("error: {}", e)));
                    }
                }
            }

            // Reset activity to avoid rapid-fire musings
            activity.touch();
        }
    });
}

/// Schedule a possible conversation re-entry after a conversation ends.
/// Analyzes conversation history, related memories, and user profile to craft
/// a contextual, in-character follow-up message delivered after a configurable delay.
pub fn maybe_schedule_reentry(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    memory: Arc<MemoryStore>,
    session_key: String,
    persona_soul: Option<String>,
) {
    // Read knobs for probability gate
    let (spontaneity, enabled, all, delay_secs, jitter) = {
        let k = knobs.read().unwrap();
        (
            k.spontaneity,
            k.conversation_reentry_enabled,
            k.all_proactive,
            k.reentry_delay_secs,
            k.reentry_jitter,
        )
    };

    if !all || !enabled {
        return;
    }

    // Stochastic trigger: spontaneity * 0.3 (at spontaneity=0.7 → ~21% chance)
    if !randomness::should_speak(0.3, spontaneity) {
        return;
    }

    tokio::spawn(async move {
        let delay = randomness::jitter_interval(delay_secs, jitter);
        let delay_min = delay.as_secs() / 60;
        let fire_at = Utc::now()
            + chrono::Duration::from_std(delay)
                .unwrap_or_else(|_| chrono::Duration::seconds(delay_secs as i64));
        observer.log(
            ObserverCategory::Heartbeat,
            format!(
                "Scheduling conversation re-entry in ~{}min for {}",
                delay_min, session_key
            ),
        );

        // Load conversation context (15 turns)
        let recent = memory.recent_turns(&session_key, 15).unwrap_or_default();
        if recent.len() < 2 {
            return; // need at least one exchange
        }

        let prompt = build_reentry_prompt(
            &recent, &memory, &session_key, persona_soul.as_deref(),
        );

        let schedule = Schedule::OneShot { at: fire_at };
        let (stype, sdata) = schedule.to_db();
        let next_run = schedule.next_run().map(|t| t.to_rfc3339());
        let job_name = format!("reentry:{}", session_key);
        let target = format!("session:{}", session_key);

        if let Err(e) = memory.delete_scheduled_jobs_by_name(&job_name) {
            tracing::warn!("Failed to dedup re-entry job {}: {}", job_name, e);
        }

        let job_id = Uuid::new_v4().to_string();
        if let Err(e) = memory.create_scheduled_job(
            &job_id,
            &job_name,
            &stype,
            &sdata,
            None,
            &prompt,
            &target,
            next_run.as_deref(),
        ) {
            tracing::warn!("Failed to persist re-entry job {}: {}", job_name, e);
        }
    });
}

/// Build the reentry prompt from conversation history, memories, and user profile.
fn build_reentry_prompt(
    recent: &[(String, String)],
    memory: &MemoryStore,
    session_key: &str,
    persona_soul: Option<&str>,
) -> String {
    let (summary_section, recent_section) = if recent.len() > 8 {
        let split = recent.len() - 6;
        let old_lines: Vec<String> = recent[..split]
            .iter()
            .map(|(role, content)| format!("[{}] {}", role, truncate(content, 120)))
            .collect();
        let recent_lines: Vec<String> = recent[split..]
            .iter()
            .map(|(role, content)| format!("{}: {}", role, content))
            .collect();
        (
            format!(
                "Earlier in the conversation (summarized):\n{}\n\n",
                old_lines.join("\n")
            ),
            recent_lines.join("\n"),
        )
    } else {
        let lines: Vec<String> = recent
            .iter()
            .map(|(role, content)| format!("{}: {}", role, content))
            .collect();
        (String::new(), lines.join("\n"))
    };

    let user_topics: String = recent
        .iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    let memories = memory
        .search_hybrid(&user_topics, None, 5)
        .unwrap_or_default();
    let memory_section = if memories.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = memories
            .iter()
            .map(|m| format!("- [{}] {}", m.category, m.content))
            .collect();
        format!("\n\nRelated things you know:\n{}", items.join("\n"))
    };

    let user_id = session_key.splitn(3, ':').nth(1).unwrap_or("unknown");
    let user_profile = memory.get_user_profile(user_id).unwrap_or_default();
    let profile_section = if user_profile.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = user_profile
            .iter()
            .map(|(k, v)| format!("- {}: {}", k, v))
            .collect();
        format!("\n\nAbout this person:\n{}", items.join("\n"))
    };

    let persona = persona_soul
        .map(|s| format!("{}\n\n", s))
        .unwrap_or_default();

    format!(
        r#"{persona}You had this conversation with the user a while ago:

{summary_section}Recent messages:
{recent_section}{memory_section}{profile_section}

Time has passed since this conversation ended. Think about whether there's something genuinely valuable you could follow up on:
- An unfinished thread worth revisiting
- A solution or idea that came to mind since then
- A relevant connection to something you know about them
- A helpful resource or approach they might not have considered

Write a natural, brief follow-up message (1-3 sentences) as if you're casually reaching out. Be specific — reference the actual topic. Don't be generic or vague.

If there's genuinely nothing worth following up on, respond with exactly: NO_FOLLOWUP"#,
    )
}

/// Check if an LLM response is a refusal / "nothing to say" variant.
/// Catches exact magic strings, short negatives, and common refusal phrases.
pub fn is_refusal(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();

    // Exact magic strings from prompts
    if lower.contains("nothing_to_say")
        || lower.contains("no_pattern")
        || lower.contains("no_followup")
    {
        return true;
    }

    // Very short responses that are just negatives
    if t.len() < 20 {
        let stripped = lower.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
        if matches!(
            stripped,
            "no" | "nothing" | "none" | "nope" | "n/a" | "na" | "not" | "pass"
        ) {
            return true;
        }
    }

    // Common refusal phrases
    if lower.contains("nothing to say")
        || lower.contains("nothing to share")
        || lower.contains("nothing worth")
        || lower.contains("nothing stands out")
        || lower.contains("nothing meaningful")
        || lower.contains("no meaningful")
        || lower.contains("don't have anything")
        || lower.contains("i have nothing")
        || lower.contains("no pattern")
        || lower.contains("no follow")
        || lower.contains("no insight")
    {
        return true;
    }

    false
}

/// Spawn the code structure indexer loop.
/// Periodically scans source files and stores structural information
/// (public symbols, `use`/`mod` statements, dependency graph) as memories.
pub fn spawn_code_indexer(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    tokio::spawn(async move {
        // Initial delay to let the system settle
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        loop {
            let (interval, enabled, all) = {
                let k = knobs.read().unwrap();
                (
                    k.code_indexer_interval_secs,
                    k.code_indexer_enabled,
                    k.all_proactive,
                )
            };

            if !all || !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }

            let sleep_dur = randomness::jitter_interval(interval, 0.2);
            tokio::time::sleep(sleep_dur).await;

            // Re-check knobs after sleep
            {
                let k = knobs.read().unwrap();
                if !k.all_proactive || !k.code_indexer_enabled {
                    continue;
                }
            }

            observer.log(
                ObserverCategory::AutonomousTask,
                "Starting code structure indexing",
            );

            let goal = "Scan the codebase and extract a structural index. For each source file \
                       in src/, extract: public functions, structs, enums, traits, impl blocks, \
                       and `use`/`mod` statements. Output a summary of the dependency graph \
                       between modules. Store each module's structure as a memory with \
                       category 'code_structure'.";

            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "funnel2:code_indexer",
                    None,
                    None,
                    None,
                    vec!["funnel2", "indexer"],
                )
            });
            let dispatch_span = lf_trace
                .as_ref()
                .map(|t| t.span("dispatch_scout", Some(goal)));

            let task = crate::core::AutonomousTask {
                goal: goal.to_string(),
                context: "[auto_store:code_structure] This is a scheduled code indexing task. \
                          Focus on extracting structure, not understanding logic. Be concise."
                    .to_string(),
                ghost: Some("scout".to_string()),
                target: crate::pulse::PulseTarget::Broadcast,
                lane: "self_improvement".to_string(),
                risk_tier: "medium".to_string(),
                repo: crate::kpi::default_repo_name(),
                task_id: None,
            };

            if let Err(e) = auto_tx.send(task).await {
                tracing::warn!("Code indexer: failed to dispatch task: {}", e);
            }

            if let Some(s) = dispatch_span {
                s.end(Some("task dispatched"));
            }
            if let Some(t) = lf_trace {
                t.end(None);
            }
        }
    });
}

/// Spawn the refactoring opportunity scanner loop.
/// Periodically analyzes code structure memories, tool failure patterns,
/// and system metrics to identify refactoring opportunities.
pub fn spawn_refactoring_scanner(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            let (interval, enabled, all, spontaneity) = {
                let k = knobs.read().unwrap();
                (
                    k.refactoring_scan_interval_secs,
                    k.refactoring_scan_enabled,
                    k.all_proactive,
                    k.spontaneity,
                )
            };

            if !all || !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }

            let sleep_dur = randomness::jitter_interval(interval, 0.2);
            tokio::time::sleep(sleep_dur).await;

            // Re-check
            {
                let k = knobs.read().unwrap();
                if !k.all_proactive || !k.refactoring_scan_enabled {
                    continue;
                }
            }

            observer.log(
                ObserverCategory::AutonomousTask,
                "Starting refactoring opportunity scan",
            );

            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "funnel2:refactoring_scan",
                    None,
                    None,
                    None,
                    vec!["funnel2", "refactoring"],
                )
            });

            // Gather code_structure memories
            let structure_memories = memory
                .search_hybrid("code_structure module symbols", None, 20)
                .unwrap_or_default();

            if structure_memories.is_empty() {
                observer.log(
                    ObserverCategory::AutonomousTask,
                    "Refactoring scan: no code_structure memories yet, skipping",
                );
                if let Some(t) = lf_trace {
                    t.end(Some("no code_structure memories"));
                }
                continue;
            }

            let structure_summary: Vec<String> = structure_memories
                .iter()
                .take(20)
                .map(|m| format!("- {}", truncate(&m.content, 200)))
                .collect();

            let prompt = format!(
                r#"Analyze this codebase structure and identify the single most impactful refactoring opportunity:

CODE STRUCTURE:
{}

Consider:
- Modules with too many public symbols (>20)
- Circular or overly complex dependency chains
- Duplicated patterns across modules
- Large files that should be split

If you find a clear, high-confidence refactoring opportunity, describe it in 2-3 sentences.
If nothing stands out or confidence is low, respond with exactly: NO_REFACTORING"#,
                structure_summary.join("\n")
            );

            let model_name = llm.provider_name().to_string();
            let gen = lf_trace
                .as_ref()
                .map(|t| t.generation("analyze_opportunities", &model_name, None));

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim();
                    if let Some(g) = gen {
                        g.end(Some(&truncate(trimmed, 500)), 0, 0);
                    }
                    if is_refusal(trimmed) || trimmed.to_uppercase().contains("NO_REFACTORING") {
                        observer.log(
                            ObserverCategory::AutonomousTask,
                            "Refactoring scan: no opportunities found",
                        );
                        continue;
                    }

                    // Store as memory
                    let _ = memory.store("refactoring_opportunity", trimmed, None);

                    observer.log(
                        ObserverCategory::AutonomousTask,
                        format!("Refactoring opportunity: \"{}\"", truncate(trimmed, 80)),
                    );

                    check_and_dispatch_refactoring(
                        trimmed, spontaneity, &observer, &memory,
                        &auto_tx, lf_trace,
                    ).await;
                }
                Err(e) => {
                    tracing::warn!("Refactoring scanner LLM call failed: {}", e);
                    if let Some(g) = gen {
                        g.end(Some(&format!("error: {}", e)), 0, 0);
                    }
                    if let Some(t) = lf_trace {
                        t.end(Some(&format!("error: {}", e)));
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{build_recent_memory_lines, has_similar_failure};
    use crate::memory::Memory;

    fn memory(category: &str, content: &str) -> Memory {
        Memory {
            id: "m1".to_string(),
            category: category.to_string(),
            content: content.to_string(),
            active: true,
            created_at: String::new(),
        }
    }

    #[test]
    fn build_recent_memory_lines_limits_and_formats() {
        let memories = vec![memory("one", "alpha"), memory("two", "beta")];
        let lines = build_recent_memory_lines(&memories, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "- [one] alpha");
    }

    #[test]
    fn has_similar_failure_matches_shared_terms() {
        let idea = "Improve caching of memory scanner";
        let failures = vec![memory(
            "improvement_idea",
            "previous improvement: caching memory scanner was rejected",
        )];
        assert!(has_similar_failure(idea, &failures));
    }

    #[test]
    fn has_similar_failure_ignores_unrelated_failures() {
        let idea = "Improve caching of memory scanner";
        let failures = vec![memory("improvement_idea", "tweak prompt wording")];
        assert!(!has_similar_failure(idea, &failures));
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Spawn the GitHub issue polling loop.
/// Polls for issues with configured labels, claims them, and dispatches as autonomous tasks.
pub fn spawn_issue_poller(
    config: crate::config::IssuePollingConfig,
    observer: ObserverHandle,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    if !config.enabled {
        return;
    }

    let labels = config.labels.join(",");
    let interval = std::time::Duration::from_secs(config.interval_secs);
    let max_concurrent = config.max_concurrent;
    let repos = config.repos.clone();

    tokio::spawn(async move {
        // Initial delay to let system settle
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let mut active_issues: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            tokio::time::sleep(interval).await;

            // Skip if at capacity
            if active_issues.len() >= max_concurrent as usize {
                continue;
            }

            observer.log(
                ObserverCategory::AutonomousTask,
                "Issue poller: checking for new issues",
            );

            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "issue_poll",
                    None,
                    None,
                    None,
                    vec!["issue_poll"],
                )
            });

            // Determine which repos to poll
            let repo_args: Vec<String> = if repos.is_empty() {
                vec![String::new()] // empty = current repo
            } else {
                repos.iter().map(|r| format!("--repo {}", r)).collect()
            };

            for repo_arg in &repo_args {
                let mut cmd_args = vec![
                    "issue".to_string(),
                    "list".to_string(),
                    "--label".to_string(),
                    labels.clone(),
                    "--state".to_string(),
                    "open".to_string(),
                    "--assignee".to_string(),
                    "@me".to_string(), // Exclude already-assigned
                    "--json".to_string(),
                    "number,title,body,labels,url".to_string(),
                    "--limit".to_string(),
                    "5".to_string(),
                ];

                // Invert: get issues NOT assigned to @me
                // We want unassigned issues, so use --no-assignee instead
                cmd_args = vec![
                    "issue".to_string(),
                    "list".to_string(),
                    "--label".to_string(),
                    labels.clone(),
                    "--state".to_string(),
                    "open".to_string(),
                    "--json".to_string(),
                    "number,title,body,labels,url,assignees".to_string(),
                    "--limit".to_string(),
                    "5".to_string(),
                ];

                if !repo_arg.is_empty() {
                    cmd_args.extend(repo_arg.split_whitespace().map(String::from));
                }

                let cmd_strs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
                let output = match tokio::process::Command::new("gh")
                    .args(&cmd_strs)
                    .output()
                    .await
                {
                    Ok(o) if o.status.success() => {
                        String::from_utf8_lossy(&o.stdout).to_string()
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!("Issue poller: gh issue list failed: {}", stderr);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("Issue poller: failed to run gh: {}", e);
                        continue;
                    }
                };

                let issues: Vec<serde_json::Value> = match serde_json::from_str(&output) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                for issue in &issues {
                    let number = issue["number"].as_u64().unwrap_or(0);
                    let title = issue["title"].as_str().unwrap_or("");
                    let body = issue["body"].as_str().unwrap_or("");
                    let url = issue["url"].as_str().unwrap_or("");
                    let issue_key = format!("{}#{}", repo_arg, number);

                    // Skip if already in-flight or has assignees
                    if active_issues.contains(&issue_key) {
                        continue;
                    }
                    let assignees = issue["assignees"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0);
                    if assignees > 0 {
                        continue;
                    }

                    // Capacity check
                    if active_issues.len() >= max_concurrent as usize {
                        break;
                    }

                    // Best-effort claim — comment on the issue
                    let num_str = number.to_string();
                    let _ = tokio::process::Command::new("gh")
                        .args(["issue", "comment", &num_str, "--body", "Athena is working on this issue."])
                        .output()
                        .await;

                    observer.log(
                        ObserverCategory::AutonomousTask,
                        &format!("Issue poller: claiming #{} — {}", number, title),
                    );

                    active_issues.insert(issue_key);

                    // Dispatch as autonomous task
                    let goal = format!("Resolve GitHub issue #{}: {}\n\n{}", number, title, body);
                    let context = format!("GitHub issue URL: {}\nIssue #{}", url, number);

                    let task = crate::core::AutonomousTask {
                        goal,
                        context,
                        ghost: Some("coder".to_string()),
                        target: crate::pulse::PulseTarget::Broadcast,
                        lane: "delivery".to_string(),
                        risk_tier: "medium".to_string(),
                        repo: repo_arg.clone(),
                        task_id: Some(format!("issue-{}", number)),
                    };

                    if auto_tx.send(task).await.is_err() {
                        tracing::warn!("Issue poller: auto_tx channel closed");
                        break;
                    }
                }
            }

            // Clean up completed issues (simple: remove all, they'll be re-checked)
            // In a full implementation, we'd track completion status
            if active_issues.len() >= max_concurrent as usize {
                // Keep them until next successful poll shows they're closed
            }

            if let Some(t) = lf_trace {
                t.end(Some(&format!("polled, {} active", active_issues.len())));
            }
        }
    });
}
