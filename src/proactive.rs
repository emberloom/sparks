use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::core::SessionContext;
use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::llm::{LlmProvider, Message};
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::{Pulse, PulseBus, PulseSource, PulseTarget, Urgency};
use crate::randomness;

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

            // Take last 50
            let recent: Vec<String> = memories
                .iter()
                .take(50)
                .map(|m| format!("- [{}] {}", m.category, m.content))
                .collect();

            let prompt = format!(
                r#"Review these recent memories and identify any interesting patterns, connections, or insights:

{}

If you notice a meaningful pattern worth sharing, describe it in 1-2 sentences. If nothing stands out, respond with exactly: NO_PATTERN"#,
                recent.join("\n")
            );

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

                    // Check if this pattern implies an actionable code improvement
                    let classify_prompt = format!(
                        "Does this pattern suggest a concrete code improvement? \
                         Pattern: \"{}\"\n\n\
                         If yes, describe the improvement in 1-2 sentences. \
                         If no, respond NO_ACTION.",
                        trimmed
                    );

                    let classify_gen = lf_trace
                        .as_ref()
                        .map(|t| t.generation("classify_improvement", &model_name, None));
                    let classify_msgs = vec![Message::user(&classify_prompt)];
                    if let Ok(classify_resp) = llm.chat(&classify_msgs).await {
                        if let Some(g) = classify_gen {
                            g.end(Some(&truncate(classify_resp.trim(), 500)), 0, 0);
                        }
                        let cr = classify_resp.trim();
                        if !cr.to_uppercase().contains("NO_ACTION") && !is_refusal(cr) {
                            // Spontaneity gate — lower threshold since patterns are speculative
                            if randomness::should_speak(0.2, spontaneity) {
                                // Check for past failures on similar improvements
                                let past_failures = memory
                                    .search_hybrid("improvement_idea failed", None, 5)
                                    .unwrap_or_default();
                                let lower_idea = cr.to_lowercase();
                                let similar_failure = past_failures.iter().any(|m| {
                                    let failure_lower = m.content.to_lowercase();
                                    lower_idea
                                        .split_whitespace()
                                        .filter(|w| w.len() > 5)
                                        .any(|word| failure_lower.contains(word))
                                });

                                if similar_failure {
                                    observer.log(
                                        ObserverCategory::MemoryScan,
                                        "Pattern improvement suppressed: similar past failure found",
                                    );
                                } else {
                                    let _ = memory.store("improvement_idea", cr, None);
                                    let task = crate::core::AutonomousTask {
                                        goal: format!(
                                            "Investigate this improvement idea: {}\n\n\
                                             Explore feasibility, identify affected files, and report findings.",
                                            cr
                                        ),
                                        context: "Discovered via memory pattern scan. \
                                                  Investigation only — do not make code changes."
                                            .to_string(),
                                        ghost: Some("scout".to_string()),
                                        target: crate::pulse::PulseTarget::Broadcast,
                                    };
                                    if let Err(e) = auto_tx.send(task).await {
                                        tracing::warn!("Memory scanner: failed to dispatch improvement task: {}", e);
                                    }
                                }
                            } else {
                                observer.log(
                                    ObserverCategory::StochasticRoll,
                                    format!("Pattern improvement suppressed by gate (spontaneity={:.2})", spontaneity),
                                );
                            }
                        }
                    }

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

                    // Check if this musing implies an actionable code improvement
                    let classify_prompt = format!(
                        "Does this reflection suggest a concrete code improvement? \
                         Musing: \"{}\"\n\n\
                         If yes, describe the improvement in 1-2 sentences. \
                         If no, respond NO_ACTION.",
                        trimmed
                    );
                    let classify_gen = lf_trace
                        .as_ref()
                        .map(|t| t.generation("classify_improvement", &model_name, None));
                    let classify_msgs = vec![Message::user(&classify_prompt)];
                    if let Ok(classify_resp) = llm.chat(&classify_msgs).await {
                        if let Some(g) = classify_gen {
                            g.end(Some(&truncate(classify_resp.trim(), 500)), 0, 0);
                        }
                        let cr = classify_resp.trim();
                        if !cr.to_uppercase().contains("NO_ACTION") && !is_refusal(cr) {
                            // Even lower spontaneity gate — musings are most speculative
                            if randomness::should_speak(0.15, spontaneity) {
                                // Check for past failures on similar improvements
                                let past_failures = memory
                                    .search_hybrid("improvement_idea failed", None, 5)
                                    .unwrap_or_default();
                                let lower_idea = cr.to_lowercase();
                                let similar_failure = past_failures.iter().any(|m| {
                                    let failure_lower = m.content.to_lowercase();
                                    lower_idea
                                        .split_whitespace()
                                        .filter(|w| w.len() > 5)
                                        .any(|word| failure_lower.contains(word))
                                });

                                if similar_failure {
                                    observer.log(
                                        ObserverCategory::IdleMusing,
                                        "Musing improvement suppressed: similar past failure found",
                                    );
                                } else {
                                    let _ = memory.store("improvement_idea", cr, None);
                                    let task = crate::core::AutonomousTask {
                                        goal: format!(
                                            "Investigate this improvement idea: {}\n\n\
                                             Explore feasibility, identify affected files, and report findings.",
                                            cr
                                        ),
                                        context: "Discovered via idle musing. \
                                                  Investigation only — do not make code changes."
                                            .to_string(),
                                        ghost: Some("scout".to_string()),
                                        target: crate::pulse::PulseTarget::Broadcast,
                                    };
                                    if let Err(e) = auto_tx.send(task).await {
                                        tracing::warn!(
                                            "Idle musing: failed to dispatch improvement task: {}",
                                            e
                                        );
                                    }
                                }
                            } else {
                                observer.log(
                                    ObserverCategory::StochasticRoll,
                                    format!(
                                        "Musing improvement suppressed by gate (spontaneity={:.2})",
                                        spontaneity
                                    ),
                                );
                            }
                        }
                    }

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
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    session_key: String,
    persona_soul: Option<String>,
    langfuse: SharedLangfuse,
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
        observer.log(
            ObserverCategory::Heartbeat,
            format!(
                "Scheduling conversation re-entry in ~{}min for {}",
                delay_min, session_key
            ),
        );

        tokio::time::sleep(delay).await;

        // Re-check knobs after delay
        {
            let k = knobs.read().unwrap();
            if !k.all_proactive || !k.conversation_reentry_enabled {
                return;
            }
        }

        // Load conversation context (15 turns)
        let recent = memory.recent_turns(&session_key, 15).unwrap_or_default();
        if recent.len() < 2 {
            return; // need at least one exchange
        }

        // Summarize older turns, keep last 6 in full
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

        // Extract topics from recent user messages for memory search
        let user_topics: String = recent
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, content)| content.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        // Search related memories
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

        // Extract user_id from session_key for profile lookup
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

        // Build the persona preamble
        let persona = persona_soul
            .as_deref()
            .map(|s| format!("{}\n\n", s))
            .unwrap_or_default();

        let prompt = format!(
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
        );

        let lf_trace = langfuse.as_ref().map(|lf| {
            ActiveTrace::start(
                lf.clone(),
                "funnel3:reentry",
                None,
                None,
                None,
                vec!["funnel3", "reentry"],
            )
        });
        let model_name = llm.provider_name().to_string();
        let gen = lf_trace
            .as_ref()
            .map(|t| t.generation("compose_reentry", &model_name, None));

        let messages = vec![Message::user(&prompt)];
        match llm.chat(&messages).await {
            Ok(response) => {
                let trimmed = response.trim();
                if let Some(g) = gen {
                    g.end(Some(&truncate(trimmed, 500)), 0, 0);
                }

                // Quality gate: reject refusals, too-short, or generic responses
                if is_refusal(trimmed) {
                    observer.log(
                        ObserverCategory::Heartbeat,
                        "Re-entry: nothing to follow up on",
                    );
                    if let Some(t) = lf_trace {
                        t.end(Some("suppressed"));
                    }
                    return;
                }
                if trimmed.len() < 30 {
                    observer.log(
                        ObserverCategory::Heartbeat,
                        "Re-entry: response too short, skipping",
                    );
                    if let Some(t) = lf_trace {
                        t.end(Some("suppressed"));
                    }
                    return;
                }

                observer.log(
                    ObserverCategory::Heartbeat,
                    format!("Re-entry generated: \"{}\"", truncate(trimmed, 80)),
                );

                let _ = memory.store("reentry", trimmed, None);

                let target = parse_session_target(&session_key);
                let pulse = Pulse::new(
                    PulseSource::ConversationReentry,
                    Urgency::Medium, // Medium = always delivers unless quiet hours
                    trimmed.to_string(),
                )
                .with_target(target);
                pulse_bus.send(pulse);
                if let Some(t) = lf_trace {
                    t.end(Some("pulse_sent"));
                }
            }
            Err(e) => {
                tracing::warn!("Conversation re-entry LLM call failed: {}", e);
                if let Some(g) = gen {
                    g.end(Some(&format!("error: {}", e)), 0, 0);
                }
                if let Some(t) = lf_trace {
                    t.end(Some(&format!("error: {}", e)));
                }
            }
        }
    });
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

/// Parse a session key "platform:user_id:chat_id" into a targeted PulseTarget.
fn parse_session_target(session_key: &str) -> PulseTarget {
    let parts: Vec<&str> = session_key.splitn(3, ':').collect();
    if parts.len() == 3 {
        PulseTarget::Session(SessionContext {
            platform: parts[0].to_string(),
            user_id: parts[1].to_string(),
            chat_id: parts[2].to_string(),
        })
    } else {
        PulseTarget::Broadcast
    }
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

                    // Check for past failures on similar refactorings before auto-dispatching
                    let past_failures = memory
                        .search_hybrid("refactoring_failed", None, 5)
                        .unwrap_or_default();
                    let lower_opportunity = trimmed.to_lowercase();
                    let similar_failure = past_failures.iter().any(|m| {
                        // Simple overlap check: if any significant word from the opportunity
                        // appears in a past failure, consider it similar
                        let failure_lower = m.content.to_lowercase();
                        lower_opportunity
                            .split_whitespace()
                            .filter(|w| w.len() > 5) // only check significant words
                            .any(|word| failure_lower.contains(word))
                    });

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
                            .map(|t| t.span("dispatch_coder", Some(trimmed)));
                        // Optionally dispatch as autonomous task if spontaneity is high enough
                        let task = crate::core::AutonomousTask {
                            goal: format!(
                                "Implement this refactoring: {}\n\n\
                                 Be careful, run tests after changes, and keep the refactoring minimal.",
                                trimmed
                            ),
                            context: "This is a suggested refactoring from automated analysis. \
                                      Proceed carefully and verify with tests."
                                .to_string(),
                            ghost: Some("coder".to_string()),
                            target: crate::pulse::PulseTarget::Broadcast,
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
