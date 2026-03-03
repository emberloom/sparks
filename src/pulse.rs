use chrono::Utc;
use tokio::sync::broadcast;

use crate::core::SessionContext;
use crate::observer::{ObserverCategory, ObserverHandle};

/// What triggered this pulse.
#[derive(Debug, Clone)]
pub enum PulseSource {
    Heartbeat,
    // Used in tests only
    #[cfg(test)]
    CronJob(String),
    MemoryScan,
    IdleMusing,
    ConversationReentry,
    AutonomousTask,
}

impl PulseSource {
    pub fn label(&self) -> &str {
        match self {
            Self::Heartbeat => "heartbeat",
            #[cfg(test)]
            Self::CronJob(id) => id.as_str(),
            Self::MemoryScan => "memory_scan",
            Self::IdleMusing => "idle_musing",
            Self::ConversationReentry => "conversation_reentry",
            Self::AutonomousTask => "autonomous_task",
        }
    }
}

/// How urgent the delivery is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// Store only, don't deliver.
    Silent,
    /// Deliver if stochastic gate passes.
    Low,
    /// Deliver unless quiet hours.
    Medium,
    /// Always deliver.
    High,
}

/// Where to deliver.
#[derive(Debug, Clone)]
pub enum PulseTarget {
    // SessionContext is read by the telegram feature (telegram.rs)
    #[cfg_attr(not(feature = "telegram"), allow(dead_code))]
    Session(SessionContext),
    Broadcast,
}

/// A proactive message emitted by any background task.
#[derive(Debug, Clone)]
pub struct Pulse {
    pub task_id: Option<String>,
    pub source: PulseSource,
    pub urgency: Urgency,
    pub content: String,
    pub ghost: Option<String>,
    pub target: PulseTarget,
}

impl Pulse {
    pub fn new(source: PulseSource, urgency: Urgency, content: String) -> Self {
        Self {
            task_id: None,
            source,
            urgency,
            content,
            ghost: None,
            target: PulseTarget::Broadcast,
        }
    }

    pub fn with_target(mut self, target: PulseTarget) -> Self {
        self.target = target;
        self
    }

    pub fn with_ghost(mut self, ghost: impl Into<String>) -> Self {
        self.ghost = Some(ghost.into());
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }
}

/// Broadcast channel for pulses.
#[derive(Clone)]
pub struct PulseBus {
    tx: broadcast::Sender<Pulse>,
}

impl PulseBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn send(&self, pulse: Pulse) {
        let _ = self.tx.send(pulse);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Pulse> {
        self.tx.subscribe()
    }
}

/// Decides whether a pulse should be delivered based on urgency, quiet hours, and tolerance.
pub struct PulseGate {
    pub tolerance: f32,
    pub quiet_hours: Option<(u32, u32)>,
    pub timezone_offset: i32,
}

impl PulseGate {
    /// Whether the pulse should be delivered to a frontend.
    pub fn should_deliver(&self, pulse: &Pulse) -> bool {
        match pulse.urgency {
            Urgency::Silent => false,
            Urgency::High => true,
            Urgency::Medium => !self.is_quiet_hours(),
            Urgency::Low => {
                if self.is_quiet_hours() {
                    return false;
                }
                // Stochastic gate based on tolerance
                crate::randomness::should_speak(1.0, self.tolerance)
            }
        }
    }

    fn is_quiet_hours(&self) -> bool {
        let (start, end) = match self.quiet_hours {
            Some(qh) => qh,
            None => return false,
        };
        let now = Utc::now();
        let local_hour =
            ((now.timestamp() / 3600 + self.timezone_offset as i64) % 24 + 24) as u32 % 24;

        if start <= end {
            // e.g., 8-18 (quiet during daytime)
            local_hour >= start && local_hour < end
        } else {
            // e.g., 22-8 (quiet overnight, wraps midnight)
            local_hour >= start || local_hour < end
        }
    }
}

/// Max pulse deliveries per hour (prevents feedback-loop flooding).
const MAX_DELIVERIES_PER_HOUR: usize = 4;

/// Spawn the pulse consumer task that gates and delivers pulses.
pub fn spawn_pulse_consumer(
    bus: PulseBus,
    observer: ObserverHandle,
    delivered_tx: tokio::sync::mpsc::Sender<Pulse>,
    knobs: crate::knobs::SharedKnobs,
) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        // Sliding window of recent delivery timestamps for rate limiting
        let mut delivery_times: std::collections::VecDeque<std::time::Instant> =
            std::collections::VecDeque::new();

        loop {
            let pulse = match rx.recv().await {
                Ok(p) => p,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Pulse consumer lagged by {} events", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            observer.log(
                ObserverCategory::PulseEmitted,
                format!(
                    "{} pulse: \"{}\"",
                    pulse.source.label(),
                    truncate(&pulse.content, 60)
                ),
            );

            let gate = {
                let k = knobs.read().unwrap_or_else(|e| e.into_inner());
                PulseGate {
                    tolerance: k.pulse_tolerance,
                    quiet_hours: k.quiet_hours,
                    timezone_offset: k.timezone_offset,
                }
            };

            if !gate.should_deliver(&pulse) {
                let reason = if pulse.urgency == Urgency::Silent {
                    "silent"
                } else if gate.is_quiet_hours() {
                    "quiet hours"
                } else {
                    "tolerance gate"
                };
                observer.log(
                    ObserverCategory::PulseSuppressed,
                    format!("Suppressed: {} ({})", pulse.source.label(), reason),
                );
                continue;
            }

            // Rate limit: drop old timestamps and check window
            let now = std::time::Instant::now();
            let one_hour = std::time::Duration::from_secs(3600);
            while delivery_times
                .front()
                .map_or(false, |t| now.duration_since(*t) > one_hour)
            {
                delivery_times.pop_front();
            }

            // High-urgency pulses bypass rate limit
            if pulse.urgency != Urgency::High && delivery_times.len() >= MAX_DELIVERIES_PER_HOUR {
                observer.log(
                    ObserverCategory::PulseSuppressed,
                    format!(
                        "Suppressed: {} (rate limit: {}/hr)",
                        pulse.source.label(),
                        MAX_DELIVERIES_PER_HOUR
                    ),
                );
                continue;
            }

            delivery_times.push_back(now);
            observer.log(
                ObserverCategory::PulseDelivered,
                format!("Delivered: {}", pulse.source.label()),
            );
            let _ = delivered_tx.send(pulse).await;
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
