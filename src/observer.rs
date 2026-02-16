use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::broadcast;

/// Categories of observable internal events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ObserverCategory {
    Startup,
    KnobChange,
    Heartbeat,
    CronTick,
    MoodChange,
    MemoryScan,
    StochasticRoll,
    PulseEmitted,
    PulseSuppressed,
    PulseDelivered,
    IdleMusing,
    EnergyShift,
    ChatIn,
    ChatOut,
    AutonomousTask,
    ToolUsage,
    ToolReload,
    SelfMetrics,
}

impl ObserverCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Startup => "STARTUP",
            Self::KnobChange => "KNOB",
            Self::Heartbeat => "HEARTBEAT",
            Self::CronTick => "CRON",
            Self::MoodChange => "MOOD",
            Self::MemoryScan => "MEMORY",
            Self::StochasticRoll => "STOCHASTIC",
            Self::PulseEmitted => "PULSE+",
            Self::PulseSuppressed => "PULSE_X",
            Self::PulseDelivered => "PULSE_OK",
            Self::IdleMusing => "IDLE",
            Self::EnergyShift => "ENERGY",
            Self::ChatIn => "CHAT_IN",
            Self::ChatOut => "CHAT_OUT",
            Self::AutonomousTask => "AUTO_TASK",
            Self::ToolUsage => "TOOL_USE",
            Self::ToolReload => "TOOL_RELOAD",
            Self::SelfMetrics => "SELF_METRICS",
        }
    }

    /// ANSI color code for the category.
    pub fn color(&self) -> &'static str {
        match self {
            Self::Startup => "\x1b[1;37m",       // bright white
            Self::KnobChange => "\x1b[33m",       // yellow
            Self::Heartbeat => "\x1b[36m",         // cyan
            Self::CronTick => "\x1b[33m",          // yellow
            Self::MoodChange => "\x1b[35m",        // magenta
            Self::MemoryScan => "\x1b[32m",        // green
            Self::StochasticRoll => "\x1b[2;37m",  // dim gray
            Self::PulseEmitted => "\x1b[1;37m",    // white bold
            Self::PulseSuppressed => "\x1b[2m",    // dim
            Self::PulseDelivered => "\x1b[1;32m",  // bright green
            Self::IdleMusing => "\x1b[34m",        // blue
            Self::EnergyShift => "\x1b[2;33m",     // yellow dim
            Self::ChatIn => "\x1b[1;34m",           // bright blue
            Self::ChatOut => "\x1b[1;35m",          // bright magenta
            Self::AutonomousTask => "\x1b[1;33m",   // bright yellow
            Self::ToolUsage => "\x1b[36m",             // cyan
            Self::ToolReload => "\x1b[1;33m",          // bright yellow
            Self::SelfMetrics => "\x1b[2;36m",         // dim cyan
        }
    }
}

/// A single observable event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserverEvent {
    pub timestamp: String,
    pub category: ObserverCategory,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ObserverEvent {
    pub fn new(category: ObserverCategory, message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now().format("%H:%M:%S").to_string(),
            category,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    /// Format for terminal display with ANSI colors.
    pub fn format_colored(&self) -> String {
        let reset = "\x1b[0m";
        let color = self.category.color();
        let label = self.category.label();
        match &self.details {
            Some(d) => format!(
                "{color}[{}] {:<12}{reset} {} ({})",
                self.timestamp, label, self.message, d
            ),
            None => format!(
                "{color}[{}] {:<12}{reset} {}",
                self.timestamp, label, self.message
            ),
        }
    }
}

/// Clonable handle for emitting observer events.
#[derive(Clone)]
pub struct ObserverHandle {
    tx: broadcast::Sender<ObserverEvent>,
}

impl ObserverHandle {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit an event. Silently drops if no receivers are listening.
    pub fn emit(&self, event: ObserverEvent) {
        let _ = self.tx.send(event);
    }

    /// Emit a simple event with just category and message.
    pub fn log(&self, category: ObserverCategory, message: impl Into<String>) {
        self.emit(ObserverEvent::new(category, message));
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<ObserverEvent> {
        self.tx.subscribe()
    }
}

/// Return the default observer socket path.
pub fn socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".athena")
        .join("observer.sock")
}

/// Spawn a UDS listener that streams observer events to connected clients as JSON lines.
pub fn spawn_uds_listener(observer: ObserverHandle) {
    let path = socket_path();

    tokio::spawn(async move {
        // Clean up stale socket
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("Failed to bind observer socket at {}: {}", path.display(), e);
                return;
            }
        };

        // Restrict socket permissions to owner only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        tracing::debug!("Observer UDS listener started at {}", path.display());

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let mut rx = observer.subscribe();
                    tokio::spawn(async move {
                        let (_, mut writer) = tokio::io::split(stream);
                        while let Ok(event) = rx.recv().await {
                            let mut line = match serde_json::to_string(&event) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            line.push('\n');
                            if writer.write_all(line.as_bytes()).await.is_err() {
                                break; // client disconnected
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("Observer socket accept error: {}", e);
                }
            }
        }
    });
}
