use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::core::{AutonomousTask, SessionContext};
use crate::kpi;
use crate::knobs::SharedKnobs;
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::PulseTarget;
use crate::randomness;

/// How a job is scheduled.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Fire once at a specific time, then disable.
    OneShot { at: DateTime<Utc> },
    /// Fire every N seconds with jitter.
    Interval { every_secs: u64, jitter: f64 },
    /// Cron expression (e.g., "0 9 * * MON-FRI").
    Cron { expression: String },
}

impl Schedule {
    /// Compute the next run time from now.
    pub fn next_run(&self) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        match self {
            Self::OneShot { at } => {
                if *at > now {
                    Some(*at)
                } else {
                    None
                }
            }
            Self::Interval { every_secs, jitter } => {
                let dur = randomness::jitter_interval(*every_secs, *jitter);
                Some(
                    now + chrono::Duration::from_std(dur)
                        .unwrap_or(chrono::Duration::seconds(*every_secs as i64)),
                )
            }
            Self::Cron { expression } => match cron::Schedule::from_str(expression) {
                Ok(schedule) => schedule.upcoming(Utc).next(),
                Err(_) => None,
            },
        }
    }

    /// Serialize for DB storage.
    pub fn to_db(&self) -> (String, String) {
        match self {
            Self::OneShot { at } => ("oneshot".into(), at.to_rfc3339()),
            Self::Interval { every_secs, jitter } => {
                ("interval".into(), format!("{}:{}", every_secs, jitter))
            }
            Self::Cron { expression } => ("cron".into(), expression.clone()),
        }
    }

    /// Deserialize from DB.
    pub fn from_db(schedule_type: &str, schedule_data: &str) -> Option<Self> {
        match schedule_type {
            "oneshot" => {
                let at = DateTime::parse_from_rfc3339(schedule_data)
                    .ok()?
                    .with_timezone(&Utc);
                Some(Self::OneShot { at })
            }
            "interval" => {
                let parts: Vec<&str> = schedule_data.split(':').collect();
                let every_secs = parts.first()?.parse().ok()?;
                let jitter = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.1);
                Some(Self::Interval { every_secs, jitter })
            }
            "cron" => Some(Self::Cron {
                expression: schedule_data.to_string(),
            }),
            _ => None,
        }
    }
}

/// A scheduled job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: Schedule,
    pub ghost: Option<String>,
    pub prompt: String,
    pub target: String,
    pub enabled: bool,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
}

/// CRUD operations and tick loop for scheduled jobs.
pub struct CronEngine {
    memory: Arc<MemoryStore>,
    observer: ObserverHandle,
    auto_tx: mpsc::Sender<AutonomousTask>,
    knobs: SharedKnobs,
}

impl CronEngine {
    pub fn new(
        memory: Arc<MemoryStore>,
        observer: ObserverHandle,
        auto_tx: mpsc::Sender<AutonomousTask>,
        knobs: SharedKnobs,
    ) -> Self {
        Self {
            memory,
            observer,
            auto_tx,
            knobs,
        }
    }

    /// Create a new job in the database.
    pub fn create_job(
        &self,
        name: &str,
        schedule: Schedule,
        prompt: &str,
        ghost: Option<&str>,
        target: &str,
    ) -> crate::error::Result<String> {
        let id = Uuid::new_v4().to_string();
        let (stype, sdata) = schedule.to_db();
        let next = schedule.next_run().map(|t| t.to_rfc3339());
        self.memory.create_scheduled_job(
            &id,
            name,
            &stype,
            &sdata,
            ghost,
            prompt,
            target,
            next.as_deref(),
        )?;
        Ok(id)
    }

    /// List all jobs.
    pub fn list_jobs(&self) -> crate::error::Result<Vec<Job>> {
        self.memory.list_scheduled_jobs()
    }

    /// Delete a job by ID.
    pub fn delete_job(&self, id: &str) -> crate::error::Result<bool> {
        self.memory.delete_scheduled_job(id)
    }

    /// Toggle a job's enabled state.
    pub fn toggle_job(&self, id: &str, enabled: bool) -> crate::error::Result<bool> {
        self.memory.toggle_scheduled_job(id, enabled)
    }

    /// Spawn the tick loop that checks for and fires due jobs.
    pub fn spawn_tick_loop(self: Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut last_cleanup = Instant::now();
            loop {
                if this.auto_tx.is_closed() {
                    this.observer.log(
                        ObserverCategory::CronTick,
                        "Cron engine stopping: autonomous task channel closed",
                    );
                    break;
                }

                if last_cleanup.elapsed() >= std::time::Duration::from_secs(3600) {
                    match this.memory.cleanup_stale_disabled_oneshots() {
                        Ok(n) if n > 0 => this.observer.log(
                            ObserverCategory::CronTick,
                            format!("Cleaned up {} stale disabled oneshot job(s)", n),
                        ),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("Oneshot cleanup failed: {}", e),
                    }
                    last_cleanup = Instant::now();
                }
                let active = {
                    let k = this.knobs.read().unwrap();
                    k.all_proactive && k.cron_enabled
                };
                if !active {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                let sleep_dur = randomness::jitter_interval(30, 0.2);
                tokio::time::sleep(sleep_dur).await;

                match this.memory.due_scheduled_jobs() {
                    Ok(jobs) if !jobs.is_empty() => {
                        this.observer.log(
                            ObserverCategory::CronTick,
                            format!(
                                "Fired {} jobs: {}",
                                jobs.len(),
                                jobs.iter()
                                    .map(|j| format!("\"{}\"", j.name))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        );
                        for job in jobs {
                            this.fire_job(&job).await;
                        }
                    }
                    Ok(_) => {} // no due jobs
                    Err(e) => {
                        tracing::warn!("Cron engine error: {}", e);
                    }
                }
            }
        });
    }

    async fn fire_job(&self, job: &Job) {
        let schedule_type = match &job.schedule {
            Schedule::OneShot { .. } => "oneshot",
            Schedule::Interval { .. } => "interval",
            Schedule::Cron { .. } => "cron",
        };

        let is_reentry = job.name.starts_with("reentry:");
        if is_reentry {
            let (all, enabled) = {
                let k = self.knobs.read().unwrap();
                (k.all_proactive, k.conversation_reentry_enabled)
            };
            if !all || !enabled {
                if matches!(job.schedule, Schedule::OneShot { .. }) {
                    let now = Utc::now().to_rfc3339();
                    let _ = self.memory.update_job_run(&job.id, None, &now, true);
                }
                self.observer.log(
                    ObserverCategory::CronTick,
                    format!("Skipped reentry job '{}' (disabled by knobs)", job.name),
                );
                return;
            }
        }

        let lane = if is_reentry { "reentry" } else { "scheduling" };
        let task = AutonomousTask {
            goal: job.prompt.clone(),
            context: format!("Scheduled job '{}' ({})", job.name, schedule_type),
            ghost: job.ghost.clone(),
            target: parse_pulse_target(&job.target),
            lane: lane.to_string(),
            risk_tier: "low".to_string(),
            repo: kpi::default_repo_name(),
            task_id: None,
        };

        match self.auto_tx.send(task).await {
            Ok(_) => {
                let next = job.schedule.next_run().map(|t| t.to_rfc3339());
                let now = Utc::now().to_rfc3339();
                let disable = matches!(job.schedule, Schedule::OneShot { .. });
                let _ = self
                    .memory
                    .update_job_run(&job.id, next.as_deref(), &now, disable);
            }
            Err(e) => {
                tracing::warn!(
                    "Cron job '{}' enqueue failed (auto task channel closed): {}",
                    job.name,
                    e
                );
            }
        }
    }
}

fn parse_pulse_target(target: &str) -> PulseTarget {
    let trimmed = target.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("broadcast") {
        return PulseTarget::Broadcast;
    }
    if let Some(rest) = trimmed.strip_prefix("session:") {
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        if parts.len() == 3 {
            return PulseTarget::Session(SessionContext {
                platform: parts[0].to_string(),
                user_id: parts[1].to_string(),
                chat_id: parts[2].to_string(),
            });
        }
    }
    tracing::warn!("Invalid pulse target '{}', defaulting to broadcast", target);
    PulseTarget::Broadcast
}
