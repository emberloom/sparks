use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use sysinfo::{Pid, System};

use crate::knobs::SharedKnobs;
use crate::langfuse::{ActiveTrace, SharedLangfuse};
use crate::memory::MemoryStore;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::randomness;
use crate::tool_usage::ToolUsageStore;

/// Runtime system metrics collected periodically.
#[derive(Debug, Clone)]
pub struct SystemMetrics {
    /// Process resident set size in bytes.
    pub rss_bytes: u64,
    /// Process CPU usage as a percentage (0.0–100.0).
    pub cpu_percent: f32,
    /// Number of active Docker containers (Athena-managed).
    pub active_containers: u64,
    /// Number of active autonomous tasks in flight.
    pub active_tasks: u64,
    /// Process uptime in seconds.
    pub uptime_secs: u64,
    /// 1-hour rolling error rate (fraction 0.0–1.0).
    pub error_rate_1h: f64,
    /// Tool failure rate from ToolUsageStore (fraction 0.0–1.0).
    pub tool_failure_rate: f64,
    /// Average LLM latency in milliseconds (rolling).
    pub llm_latency_avg_ms: u64,
    /// Total number of stored memories.
    pub memory_count: u64,
    /// Database file size in bytes.
    pub db_size_bytes: u64,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self {
            rss_bytes: 0,
            cpu_percent: 0.0,
            active_containers: 0,
            active_tasks: 0,
            uptime_secs: 0,
            error_rate_1h: 0.0,
            tool_failure_rate: 0.0,
            llm_latency_avg_ms: 0,
            memory_count: 0,
            db_size_bytes: 0,
        }
    }
}

impl SystemMetrics {
    /// Format a compact summary for injection into prompts.
    pub fn summary(&self) -> String {
        format!(
            "System: RSS={:.1}MB CPU={:.1}% containers={} tasks={} uptime={}s \
             error_rate={:.2} tool_fail={:.2} llm_latency={}ms memories={} db={:.1}MB",
            self.rss_bytes as f64 / 1_048_576.0,
            self.cpu_percent,
            self.active_containers,
            self.active_tasks,
            self.uptime_secs,
            self.error_rate_1h,
            self.tool_failure_rate,
            self.llm_latency_avg_ms,
            self.memory_count,
            self.db_size_bytes as f64 / 1_048_576.0,
        )
    }
}

pub type SharedMetrics = Arc<RwLock<SystemMetrics>>;

/// Global LLM latency tracker — updated by LLM providers, read by metrics collector.
pub static LLM_LATENCY_AVG_MS: AtomicU64 = AtomicU64::new(0);
/// Running count of LLM calls (for averaging).
pub static LLM_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
/// Running total of LLM latency ms (for averaging).
pub static LLM_LATENCY_TOTAL_MS: AtomicU64 = AtomicU64::new(0);

/// Active autonomous tasks currently in flight.
pub static ACTIVE_TASKS: AtomicU64 = AtomicU64::new(0);

/// Rolling error counters for 1-hour error rate.
/// These are approximate — reset every hour by the metrics collector.
pub static ERROR_COUNT: AtomicU64 = AtomicU64::new(0);
pub static CALL_COUNT: AtomicU64 = AtomicU64::new(0);
/// Epoch second of the last error counter reset.
static ERROR_WINDOW_START: AtomicU64 = AtomicU64::new(0);

/// Record an LLM call latency. Called from LLM providers.
pub fn record_llm_latency(latency_ms: u64) {
    let total = LLM_LATENCY_TOTAL_MS.fetch_add(latency_ms, Ordering::Relaxed) + latency_ms;
    let count = LLM_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count > 0 {
        LLM_LATENCY_AVG_MS.store(total / count, Ordering::Relaxed);
    }
}

/// Increment active task count. Call when starting an autonomous task.
pub fn inc_active_tasks() {
    ACTIVE_TASKS.fetch_add(1, Ordering::Relaxed);
}

/// Decrement active task count. Call when an autonomous task completes.
pub fn dec_active_tasks() {
    ACTIVE_TASKS.fetch_sub(1, Ordering::Relaxed);
}

/// Record a successful LLM/tool call (for error rate tracking).
pub fn record_call() {
    CALL_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Record a failed LLM/tool call (for error rate tracking).
pub fn record_error() {
    CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Compute the 1-hour rolling error rate and reset counters if the window has expired.
fn compute_error_rate_1h() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let window_start = ERROR_WINDOW_START.load(Ordering::Relaxed);
    if window_start == 0 {
        ERROR_WINDOW_START.store(now, Ordering::Relaxed);
    }

    // Reset counters every hour
    if now.saturating_sub(window_start) >= 3600 {
        ERROR_WINDOW_START.store(now, Ordering::Relaxed);
        ERROR_COUNT.store(0, Ordering::Relaxed);
        CALL_COUNT.store(0, Ordering::Relaxed);
        return 0.0;
    }

    let errors = ERROR_COUNT.load(Ordering::Relaxed);
    let calls = CALL_COUNT.load(Ordering::Relaxed);
    if calls > 0 {
        errors as f64 / calls as f64
    } else {
        0.0
    }
}

/// Anomaly thresholds for auto-dispatching health alerts.
const ANOMALY_TOOL_FAILURE_RATE: f64 = 0.3;
const ANOMALY_LLM_LATENCY_MS: u64 = 5000;
const ANOMALY_ERROR_RATE: f64 = 0.2;
/// Minimum uptime before anomaly detection kicks in (avoid false positives at startup).
const ANOMALY_MIN_UPTIME_SECS: u64 = 300;

/// Spawn the periodic metrics collector task.
pub fn spawn_metrics_collector(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    metrics: SharedMetrics,
    memory: Arc<MemoryStore>,
    usage_store: Arc<ToolUsageStore>,
    db_path: std::path::PathBuf,
    auto_tx: tokio::sync::mpsc::Sender<crate::core::AutonomousTask>,
    langfuse: SharedLangfuse,
) {
    let start_time = std::time::Instant::now();

    tokio::spawn(async move {
        // sysinfo::System for process metrics
        let mut sys = System::new();
        let pid = Pid::from_u32(std::process::id());

        loop {
            let (interval, enabled, all) = {
                let k = knobs.read().unwrap();
                (k.metrics_interval_secs, k.self_dev_enabled, k.all_proactive)
            };

            if !all || !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let dur = randomness::jitter_interval(interval, 0.1);
            tokio::time::sleep(dur).await;

            // Re-check knobs after sleep
            {
                let k = knobs.read().unwrap();
                if !k.all_proactive || !k.self_dev_enabled {
                    continue;
                }
            }

            // Refresh process info
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);

            let (rss, cpu) = sys
                .process(pid)
                .map(|p| (p.memory(), p.cpu_usage()))
                .unwrap_or((0, 0.0));

            // Container count — count via bollard if available, else 0
            let active_containers = count_containers().await;

            // Tool failure rate
            let tool_failure_rate = usage_store
                .all()
                .ok()
                .map(|tools| {
                    let total: u64 = tools.iter().map(|t| t.invocation_count).sum();
                    let failures: u64 = tools.iter().map(|t| t.failure_count).sum();
                    if total > 0 {
                        failures as f64 / total as f64
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);

            // Memory count
            let memory_count = memory.list().map(|m| m.len() as u64).unwrap_or(0);

            // DB file size
            let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

            // LLM latency
            let llm_latency = LLM_LATENCY_AVG_MS.load(Ordering::Relaxed);

            // Active tasks from global counter
            let active_tasks = ACTIVE_TASKS.load(Ordering::Relaxed);

            // Error rate from rolling window
            let error_rate_1h = compute_error_rate_1h();

            let uptime = start_time.elapsed().as_secs();

            let new_metrics = SystemMetrics {
                rss_bytes: rss,
                cpu_percent: cpu,
                active_containers,
                active_tasks,
                uptime_secs: uptime,
                error_rate_1h,
                tool_failure_rate,
                llm_latency_avg_ms: llm_latency,
                memory_count,
                db_size_bytes: db_size,
            };

            let metrics_summary = new_metrics.summary();
            observer.emit(crate::observer::ObserverEvent::new(
                ObserverCategory::SelfMetrics,
                metrics_summary.clone(),
            ));

            // Langfuse trace per metrics cycle
            let lf_trace = langfuse.as_ref().map(|lf| {
                ActiveTrace::start(
                    lf.clone(),
                    "funnel1:health_monitor",
                    None,
                    None,
                    None,
                    vec!["funnel1", "metrics"],
                )
            });

            // Anomaly detection — dispatch health alerts when thresholds are breached
            if uptime > ANOMALY_MIN_UPTIME_SECS {
                let mut anomalies = Vec::new();

                if tool_failure_rate > ANOMALY_TOOL_FAILURE_RATE {
                    anomalies.push(format!(
                        "Tool failure rate {:.1}% exceeds threshold {:.0}%",
                        tool_failure_rate * 100.0,
                        ANOMALY_TOOL_FAILURE_RATE * 100.0
                    ));
                }
                if llm_latency > ANOMALY_LLM_LATENCY_MS {
                    anomalies.push(format!(
                        "LLM latency {}ms exceeds threshold {}ms",
                        llm_latency, ANOMALY_LLM_LATENCY_MS
                    ));
                }
                if error_rate_1h > ANOMALY_ERROR_RATE {
                    anomalies.push(format!(
                        "Error rate {:.1}% exceeds threshold {:.0}%",
                        error_rate_1h * 100.0,
                        ANOMALY_ERROR_RATE * 100.0
                    ));
                }

                if !anomalies.is_empty() {
                    let alert_msg = anomalies.join("; ");

                    // Langfuse anomaly span
                    let anomaly_span = lf_trace
                        .as_ref()
                        .map(|t| t.span("anomaly_check", Some(&metrics_summary)));
                    if let Some(s) = anomaly_span {
                        s.end(Some(&format!("ANOMALY: {}", alert_msg)));
                    }
                    let dispatch_span = lf_trace
                        .as_ref()
                        .map(|t| t.span("dispatch_diagnostic", Some(&alert_msg)));

                    observer.log(
                        ObserverCategory::SelfMetrics,
                        format!("ANOMALY DETECTED: {}", alert_msg),
                    );

                    let task = crate::core::AutonomousTask {
                        goal: format!(
                            "Health anomaly detected: {}. Investigate the root cause. \
                             Check recent tool failures, LLM provider status, and error logs. \
                             Suggest a fix or mitigation.",
                            alert_msg
                        ),
                        context: format!("Current metrics: {}", metrics_summary),
                        ghost: Some("scout".to_string()),
                        target: crate::pulse::PulseTarget::Broadcast,
                        lane: "self_improvement".to_string(),
                        risk_tier: "high".to_string(),
                        repo: crate::kpi::default_repo_name(),
                    };
                    let _ = auto_tx.send(task).await;

                    if let Some(s) = dispatch_span {
                        s.end(Some("dispatched to scout"));
                    }
                } else if let Some(ref t) = lf_trace {
                    let s = t.span("anomaly_check", Some(&metrics_summary));
                    s.end(Some("healthy"));
                }
            }

            // End Langfuse trace
            if let Some(t) = lf_trace {
                t.end(Some(&metrics_summary));
            }

            if let Ok(mut m) = metrics.write() {
                *m = new_metrics;
            }
        }
    });
}

/// Count active Athena containers via bollard.
async fn count_containers() -> u64 {
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(_) => return 0,
    };

    let mut filters = std::collections::HashMap::new();
    filters.insert("label", vec!["managed_by=athena"]);

    let opts = bollard::container::ListContainersOptions {
        filters,
        ..Default::default()
    };

    docker
        .list_containers(Some(opts))
        .await
        .map(|c| c.len() as u64)
        .unwrap_or(0)
}
