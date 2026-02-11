use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::knobs::SharedKnobs;
use crate::llm::{LlmProvider, Message};
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::{Pulse, PulseBus, PulseSource, Urgency};
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

            // Load recent memories
            let memories = match memory.list() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Memory scanner: failed to list: {}", e);
                    continue;
                }
            };

            if memories.is_empty() {
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

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim();
                    if trimmed == "NO_PATTERN" || trimmed.contains("NO_PATTERN") {
                        observer.log(ObserverCategory::MemoryScan, "No patterns found");
                        continue;
                    }

                    // Stochastic gate
                    if randomness::should_speak(0.6, spontaneity) {
                        let pulse = Pulse::new(
                            PulseSource::MemoryScan,
                            Urgency::Low,
                            trimmed.to_string(),
                        );
                        pulse_bus.send(pulse);
                    } else {
                        observer.log(
                            ObserverCategory::StochasticRoll,
                            format!("Memory pattern suppressed by gate (spontaneity={:.2})", spontaneity),
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("Memory scanner LLM call failed: {}", e);
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
) {
    tokio::spawn(async move {
        loop {
            // Check every ~5 min
            tokio::time::sleep(randomness::jitter_interval(300, 0.3)).await;

            let (threshold, enabled, all) = {
                let k = knobs.read().unwrap();
                (k.idle_threshold_secs, k.idle_musings_enabled, k.all_proactive)
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

            // Sample random memories for reflection
            let memories = match memory.list() {
                Ok(m) => m,
                Err(_) => continue,
            };

            if memories.is_empty() {
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

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim().to_string();
                    observer.log(
                        ObserverCategory::IdleMusing,
                        format!("Generated musing: \"{}\"", truncate(&trimmed, 60)),
                    );

                    // Store as memory
                    let _ = memory.store("musing", &trimmed, None);

                    let pulse = Pulse::new(
                        PulseSource::IdleMusing,
                        Urgency::Low,
                        trimmed,
                    );
                    pulse_bus.send(pulse);
                }
                Err(e) => {
                    tracing::warn!("Idle musing LLM call failed: {}", e);
                }
            }

            // Reset activity to avoid rapid-fire musings
            activity.touch();
        }
    });
}

/// Schedule a possible conversation re-entry after a conversation ends.
/// Call this after each conversation interaction with a 15% chance.
pub fn maybe_schedule_reentry(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    session_key: String,
) {
    let mut rng = rand::thread_rng();
    use rand::Rng;
    if rng.gen::<f32>() > 0.15 {
        return;
    }

    tokio::spawn(async move {
        {
            let k = knobs.read().unwrap();
            if !k.all_proactive || !k.conversation_reentry_enabled {
                return;
            }
        }

        let delay = randomness::reentry_delay();
        observer.log(
            ObserverCategory::Heartbeat,
            format!("Scheduling conversation re-entry in {}h", delay.as_secs() / 3600),
        );

        tokio::time::sleep(delay).await;

        // Re-check knobs
        {
            let k = knobs.read().unwrap();
            if !k.all_proactive || !k.conversation_reentry_enabled {
                return;
            }
        }

        // Load conversation context
        let recent = memory.recent_turns(&session_key, 5).unwrap_or_default();
        if recent.is_empty() {
            return;
        }

        let context: Vec<String> = recent
            .iter()
            .map(|(role, content)| format!("{}: {}", role, truncate(content, 100)))
            .collect();

        let prompt = format!(
            r#"You had this conversation earlier:

{}

Is there a meaningful follow-up thought or question you could share? Something that adds value, not just small talk.
If yes, write it (1-2 sentences). If not, respond with: NO_FOLLOWUP"#,
            context.join("\n")
        );

        let messages = vec![Message::user(&prompt)];
        match llm.chat(&messages).await {
            Ok(response) => {
                let trimmed = response.trim();
                if trimmed == "NO_FOLLOWUP" || trimmed.contains("NO_FOLLOWUP") {
                    return;
                }
                let pulse = Pulse::new(
                    PulseSource::ConversationReentry,
                    Urgency::Low,
                    trimmed.to_string(),
                );
                pulse_bus.send(pulse);
            }
            Err(e) => {
                tracing::warn!("Conversation re-entry LLM call failed: {}", e);
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
