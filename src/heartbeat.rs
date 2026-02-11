use std::path::PathBuf;
use std::sync::Arc;

use crate::knobs::SharedKnobs;
use crate::llm::{LlmProvider, Message};
use crate::memory::MemoryStore;
use crate::mood::MoodState;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::{Pulse, PulseBus, PulseSource, Urgency};
use crate::randomness;

/// Load and parse bullet points from a HEARTBEAT.md file.
fn load_heartbeat_items(path: &Option<String>) -> Vec<String> {
    let path = match path {
        Some(p) => resolve_path(p),
        None => {
            // Default location
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".athena")
                .join("souls")
                .join("HEARTBEAT.md")
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("No HEARTBEAT.md found at {}: {}", path.display(), e);
            return vec![];
        }
    };

    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("- ") || l.starts_with("* "))
        .map(|l| l[2..].trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn resolve_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(&path[2..])
    } else {
        PathBuf::from(path)
    }
}

/// Spawn the heartbeat background loop.
pub fn spawn_heartbeat_loop(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    mood: Arc<MoodState>,
    soul_file: Option<String>,
) {
    tokio::spawn(async move {
        loop {
            let (interval, jitter, enabled, all) = {
                let k = knobs.read().unwrap();
                (
                    k.heartbeat_interval_secs,
                    k.heartbeat_jitter,
                    k.heartbeat_enabled,
                    k.all_proactive,
                )
            };

            if !all || !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let sleep_dur = randomness::jitter_interval(interval, jitter);
            observer.log(
                ObserverCategory::Heartbeat,
                format!("Sleeping for {}s until next heartbeat", sleep_dur.as_secs()),
            );
            tokio::time::sleep(sleep_dur).await;

            // Re-check knobs after sleeping
            {
                let k = knobs.read().unwrap();
                if !k.all_proactive || !k.heartbeat_enabled {
                    continue;
                }
            }

            observer.log(ObserverCategory::Heartbeat, "Heartbeat tick - waking up");

            // 1. Load HEARTBEAT.md items and sample a subset
            let items = load_heartbeat_items(&soul_file);
            let sampled = if items.is_empty() {
                vec!["Reflect on recent conversations and whether anything needs follow-up.".to_string()]
            } else {
                let sample_count = (items.len() / 3).max(1);
                let indices = randomness::sample_indices(sample_count, items.len());
                let mut sampled: Vec<String> = indices.into_iter().map(|i| items[i].clone()).collect();
                sampled.push("Wildcard: think about something unexpected or creative.".to_string());
                observer.log(
                    ObserverCategory::Heartbeat,
                    format!("Sampled {}/{} items + wildcard", sample_count, items.len()),
                );
                sampled
            };

            // 2. Curiosity dice: sample random memories
            let curiosity_memories = match memory.list() {
                Ok(all) if !all.is_empty() => {
                    let indices = randomness::sample_indices(3.min(all.len()), all.len());
                    let picked: Vec<String> = indices
                        .into_iter()
                        .map(|i| format!("- [{}] {}", all[i].category, all[i].content))
                        .collect();
                    observer.log(
                        ObserverCategory::StochasticRoll,
                        format!("Curiosity dice: pulled {} random memories", picked.len()),
                    );
                    picked.join("\n")
                }
                _ => String::new(),
            };

            // 3. Get mood context
            let mood_desc = mood.describe();

            // 4. Build prompt and ask LLM
            let initiatives = sampled
                .iter()
                .map(|s| format!("- {}", s))
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = format!(
                r#"You are in heartbeat mode — a periodic reflection cycle. You may choose to share something with the user, or stay silent.

Current state:
{}

Random initiatives to consider (pick at most one, or none):
{}

Random memories that surfaced:
{}

Based on all this, do you have anything worth sharing? If yes, write a brief, natural message (1-3 sentences). If nothing feels worth saying right now, respond with exactly: NOTHING_TO_SAY"#,
                mood_desc, initiatives, curiosity_memories
            );

            let messages = vec![Message::user(&prompt)];
            match llm.chat(&messages).await {
                Ok(response) => {
                    let trimmed = response.trim();
                    if trimmed == "NOTHING_TO_SAY" || trimmed.contains("NOTHING_TO_SAY") {
                        observer.log(
                            ObserverCategory::Heartbeat,
                            "Heartbeat: nothing to say (suppressed)",
                        );
                    } else {
                        let pulse = Pulse::new(
                            PulseSource::Heartbeat,
                            Urgency::Low,
                            trimmed.to_string(),
                        );
                        pulse_bus.send(pulse);
                    }
                }
                Err(e) => {
                    tracing::warn!("Heartbeat LLM call failed: {}", e);
                }
            }
        }
    });
}
