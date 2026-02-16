use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::llm::{LlmProvider, Message};
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::pulse::{Pulse, PulseBus, PulseSource, Urgency};
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
    pulse_bus: PulseBus,
    llm: Arc<dyn LlmProvider>,
    knobs: SharedKnobs,
    langfuse: SharedLangfuse,
}

impl CronEngine {
    pub fn new(
        memory: Arc<MemoryStore>,
        observer: ObserverHandle,
        pulse_bus: PulseBus,
        llm: Arc<dyn LlmProvider>,
        knobs: SharedKnobs,
        langfuse: SharedLangfuse,
    ) -> Self {
        Self {
            memory,
            observer,
            pulse_bus,
            llm,
            knobs,
            langfuse,
        }
    }

    /// Create a new job in the database.
    pub fn create_job(
        &self,
        name: &str,
        schedule: Schedule,
        prompt: &str,
        ghost: Option<&str>,
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
            loop {
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

        let lf_trace = self.langfuse.as_ref().map(|lf| {
            ActiveTrace::start(
                lf.clone(),
                &format!("cron:{}", job.name),
                None,
                None,
                Some(&job.prompt),
                vec!["scheduling", "cron", schedule_type],
            )
        });

        let model_name = self.llm.provider_name();
        let gen = lf_trace
            .as_ref()
            .map(|t| t.generation("cron_llm", model_name, Some(&job.prompt)));

        let messages = vec![Message::user(&job.prompt)];
        match self.llm.chat(&messages).await {
            Ok(response) => {
                let response_trimmed = response.trim().to_string();
                if let Some(g) = gen {
                    let preview = if response_trimmed.len() > 500 {
                        &response_trimmed[..response_trimmed.floor_char_boundary(500)]
                    } else {
                        &response_trimmed
                    };
                    g.end(Some(preview), 0, 0);
                }

                let _ = self.memory.store("cron", &response_trimmed, None);
                let pulse = Pulse::new(
                    PulseSource::CronJob(job.name.clone()),
                    Urgency::Medium,
                    response_trimmed,
                );
                self.pulse_bus.send(pulse);

                let next = job.schedule.next_run().map(|t| t.to_rfc3339());
                let now = Utc::now().to_rfc3339();
                let disable = matches!(job.schedule, Schedule::OneShot { .. });
                let _ = self
                    .memory
                    .update_job_run(&job.id, next.as_deref(), &now, disable);

                if let Some(t) = lf_trace {
                    t.end(Some("completed"));
                }
            }
            Err(e) => {
                tracing::warn!("Cron job '{}' LLM call failed: {}", job.name, e);
                if let Some(g) = gen {
                    g.end(Some(&format!("error: {}", e)), 0, 0);
                }
                if let Some(t) = lf_trace {
                    t.end(Some(&format!("error: {}", e)));
                }
            }
        }
    }
}
