use chrono::Utc;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::core::SessionContext;
use crate::observer::{ObserverCategory, ObserverHandle};

/// What triggered this pulse.
#[derive(Debug, Clone)]
pub enum PulseSource {
    Heartbeat,
    CronJob(String),
    MemoryScan,
    IdleMusing,
    MoodShift,
    ConversationReentry,
}

impl PulseSource {
    pub fn label(&self) -> &str {
        match self {
            Self::Heartbeat => "heartbeat",
            Self::CronJob(id) => id.as_str(),
            Self::MemoryScan => "memory_scan",
            Self::IdleMusing => "idle_musing",
            Self::MoodShift => "mood_shift",
            Self::ConversationReentry => "conversation_reentry",
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
    Session(SessionContext),
    Broadcast,
}

/// A proactive message emitted by any background task.
#[derive(Debug, Clone)]
pub struct Pulse {
    pub id: String,
    pub source: PulseSource,
    pub urgency: Urgency,
    pub content: String,
    pub ghost: Option<String>,
    pub target: PulseTarget,
    pub timestamp: String,
}

impl Pulse {
    pub fn new(source: PulseSource, urgency: Urgency, content: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            source,
            urgency,
            content,
            ghost: None,
            target: PulseTarget::Broadcast,
            timestamp: Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
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
        let local_hour = ((now.timestamp() / 3600 + self.timezone_offset as i64) % 24 + 24) as u32 % 24;

        if start <= end {
            // e.g., 8-18 (quiet during daytime)
            local_hour >= start && local_hour < end
        } else {
            // e.g., 22-8 (quiet overnight, wraps midnight)
            local_hour >= start || local_hour < end
        }
    }
}

/// Spawn the pulse consumer task that gates and delivers pulses.
pub fn spawn_pulse_consumer(
    bus: PulseBus,
    observer: ObserverHandle,
    delivered_tx: tokio::sync::mpsc::Sender<Pulse>,
    knobs: crate::knobs::SharedKnobs,
) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
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
                format!("{} pulse: \"{}\"", pulse.source.label(), truncate(&pulse.content, 60)),
            );

            let gate = {
                let k = knobs.read().unwrap();
                PulseGate {
                    tolerance: k.pulse_tolerance,
                    quiet_hours: k.quiet_hours,
                    timezone_offset: k.timezone_offset,
                }
            };

            if gate.should_deliver(&pulse) {
                observer.log(
                    ObserverCategory::PulseDelivered,
                    format!("Delivered: {}", pulse.source.label()),
                );
                let _ = delivered_tx.send(pulse).await;
            } else {
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
