use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::kpi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct EvalCommandOptions {
    pub suite: PathBuf,
    pub scenario_filter: Option<String>,
    pub output_dir: PathBuf,
    pub history_file: PathBuf,
    pub athena_bin: PathBuf,
    pub fail_fast: bool,
    pub max_tasks: u32,
    pub baseline: Option<PathBuf>,
    pub update_baseline: bool,
    pub allow_missing_baseline: bool,
    pub max_regression: f64,
    pub output_format: EvalOutputFormat,
    pub cli_tool: Option<String>,
    pub cli_model: Option<String>,
    pub dispatch_context: Option<String>,
    pub cli_timeout_secs: u64,
    pub use_worktree: bool,
    pub keep_worktrees: bool,
}

#[derive(Debug, Serialize)]
struct EvalResultArtifact {
    schema_version: String,
    run_id: String,
    generated_at_utc: String,
    scenario: ScenarioInfo,
    provenance: ScenarioProvenance,
    harness: HarnessRunInfo,
    baseline: BaselineComparison,
    gate: GateSummary,
    artifacts: ArtifactPaths,
}

#[derive(Debug, Serialize)]
struct ScenarioInfo {
    name: String,
    version: String,
    path: String,
    filter: Option<String>,
    task_count: usize,
}

#[derive(Debug, Serialize)]
struct ScenarioProvenance {
    suite_sha256: String,
    git_commit: Option<String>,
    git_branch: Option<String>,
}

#[derive(Debug, Serialize)]
struct HarnessRunInfo {
    command: Vec<String>,
    exit_code: i32,
    gate_ok: bool,
    overall_score: Option<f64>,
    gate_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BaselineComparison {
    required: bool,
    path: String,
    baseline_found: bool,
    baseline_version: Option<String>,
    baseline_gate_ok: Option<bool>,
    baseline_overall_score: Option<f64>,
    score_delta: Option<f64>,
    max_regression: f64,
    passed: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GateSummary {
    passed: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ArtifactPaths {
    report_json: Option<String>,
    report_md: Option<String>,
    history_jsonl: Option<String>,
    normalized_json: String,
}

#[derive(Debug, Deserialize)]
struct HarnessReport {
    suite: Option<String>,
    gate_ok: bool,
    overall_score: Option<f64>,
    #[serde(default)]
    gate_reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct BaselineMetrics {
    version: Option<String>,
    gate_ok: Option<bool>,
    overall_score: Option<f64>,
}

pub fn run_eval(
    config: &Config,
    cli_config_path: Option<&Path>,
    opts: EvalCommandOptions,
) -> anyhow::Result<()> {
    let repo_root = std::env::current_dir().context("Failed to read current directory")?;
    let run_id = uuid::Uuid::new_v4().to_string();

    let suite_path = resolve_path(&repo_root, &opts.suite);
    let mut suite_json: Value = serde_json::from_str(
        &std::fs::read_to_string(&suite_path)
            .with_context(|| format!("Failed to read suite {}", suite_path.display()))?,
    )
    .with_context(|| format!("Failed to parse suite JSON {}", suite_path.display()))?;

    let scenario_filter = opts
        .scenario_filter
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(filter) = scenario_filter.as_deref() {
        apply_scenario_filter(&mut suite_json, filter)?;
    }

    let suite_name = suite_json
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let suite_version = suite_json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("v1")
        .to_string();
    let task_count = suite_json
        .get("tasks")
        .and_then(Value::as_array)
        .map(|rows| rows.len())
        .unwrap_or(0);

    let suite_bytes = serde_json::to_vec(&suite_json).context("Failed to serialize suite")?;
    let suite_sha256 = format!("{:x}", Sha256::digest(&suite_bytes));

    let out_dir = resolve_path(&repo_root, &opts.output_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create output dir {}", out_dir.display()))?;

    let suite_for_run = if scenario_filter.is_some() {
        let filtered_path = out_dir.join(format!("suite-{}-filtered.json", run_id));
        std::fs::write(
            &filtered_path,
            serde_json::to_string_pretty(&suite_json).context("Failed to format filtered suite")?,
        )
        .with_context(|| format!("Failed to write {}", filtered_path.display()))?;
        filtered_path
    } else {
        suite_path.clone()
    };

    let history_file = resolve_path(&repo_root, &opts.history_file);
    if let Some(parent) = history_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let athena_bin = resolve_path(&repo_root, &opts.athena_bin);
    let config_path = resolve_config_path(&repo_root, cli_config_path)?;

    let mut harness_args: Vec<String> = vec![
        "scripts/eval_harness.py".to_string(),
        "--suite".to_string(),
        suite_for_run.display().to_string(),
        "--config".to_string(),
        config_path.display().to_string(),
        "--athena-bin".to_string(),
        athena_bin.display().to_string(),
        "--output-dir".to_string(),
        out_dir.display().to_string(),
        "--history-file".to_string(),
        history_file.display().to_string(),
    ];

    if opts.fail_fast {
        harness_args.push("--fail-fast".to_string());
    }
    if opts.max_tasks > 0 {
        harness_args.push("--max-tasks".to_string());
        harness_args.push(opts.max_tasks.to_string());
    }
    if let Some(tool) = opts.cli_tool.as_deref() {
        harness_args.push("--cli-tool".to_string());
        harness_args.push(tool.to_string());
    }
    if let Some(model) = opts.cli_model.as_deref() {
        harness_args.push("--cli-model".to_string());
        harness_args.push(model.to_string());
    }
    if let Some(dispatch_context) = opts.dispatch_context.as_deref() {
        harness_args.push("--dispatch-context".to_string());
        harness_args.push(dispatch_context.to_string());
    }
    if opts.cli_timeout_secs > 0 {
        harness_args.push("--cli-timeout-secs".to_string());
        harness_args.push(opts.cli_timeout_secs.to_string());
    }
    if !opts.use_worktree {
        harness_args.push("--no-use-worktree".to_string());
    }
    if opts.keep_worktrees {
        harness_args.push("--keep-worktrees".to_string());
    }

    let output = Command::new("python3")
        .args(&harness_args)
        .current_dir(&repo_root)
        .output()
        .context("Failed to execute scripts/eval_harness.py")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stdout.trim().is_empty() {
        print!("{}", stdout);
    }
    if !stderr.trim().is_empty() {
        eprint!("{}", stderr);
    }

    let report_json = parse_kv_path(&stdout, "report_json=");
    let report_md = parse_kv_path(&stdout, "report_md=");
    let history_jsonl = parse_kv_path(&stdout, "history_jsonl=");

    let harness_report = report_json
        .as_deref()
        .and_then(|p| load_harness_report(Path::new(p)).ok());
    let harness_gate_ok = harness_report
        .as_ref()
        .map(|r| r.gate_ok)
        .unwrap_or_else(|| parse_gate_ok(&stdout).unwrap_or(output.status.success()));
    let harness_overall_score = harness_report
        .as_ref()
        .and_then(|r| r.overall_score)
        .or_else(|| parse_overall_score(&stdout));
    let mut harness_reasons = harness_report
        .as_ref()
        .map(|r| r.gate_reasons.clone())
        .unwrap_or_default();
    if !output.status.success() && harness_reasons.is_empty() {
        harness_reasons.push(format!(
            "eval_harness_exit_non_zero code={}",
            output.status.code().unwrap_or(1)
        ));
    }

    let baseline_path = opts.baseline.unwrap_or_else(|| {
        out_dir.join(format!(
            "baseline-{}.json",
            sanitize_filename(&suite_name)
        ))
    });
    let baseline_required = !opts.update_baseline && !opts.allow_missing_baseline;
    let baseline_metrics = load_baseline_metrics(&baseline_path).ok();

    let mut baseline_reasons = Vec::new();
    let mut baseline_ok = true;
    let mut score_delta = None;

    if baseline_metrics.is_none() && baseline_required {
        baseline_ok = false;
        baseline_reasons.push(format!(
            "baseline_missing path={} (use --update-baseline or --allow-missing-baseline)",
            baseline_path.display()
        ));
    }

    if let (Some(current), Some(base)) = (
        harness_overall_score,
        baseline_metrics.as_ref().and_then(|b| b.overall_score),
    ) {
        let delta = current - base;
        score_delta = Some(delta);
        if delta < -opts.max_regression {
            baseline_ok = false;
            baseline_reasons.push(format!(
                "overall_score_regressed current={:.3} baseline={:.3} delta={:.3} allowed_regression={:.3}",
                current, base, delta, opts.max_regression
            ));
        }
    }

    let mut gate_reasons = Vec::new();
    if !harness_gate_ok {
        gate_reasons.push("harness_gate_failed".to_string());
    }
    gate_reasons.extend(harness_reasons.clone());
    if !baseline_ok {
        gate_reasons.extend(baseline_reasons.clone());
    }
    let passed = output.status.success() && harness_gate_ok && baseline_ok;

    let now_utc = chrono::Utc::now();
    let generated_at = now_utc
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .to_string();
    let ts = now_utc.format("%Y%m%dT%H%M%SZ").to_string();
    let normalized_path = out_dir.join(format!("athena-eval-{}.json", ts));

    let (git_commit, git_branch) = git_provenance(&repo_root);
    let artifact = EvalResultArtifact {
        schema_version: "athena_eval_result_v1".to_string(),
        run_id: run_id.clone(),
        generated_at_utc: generated_at,
        scenario: ScenarioInfo {
            name: harness_report
                .as_ref()
                .and_then(|r| r.suite.clone())
                .unwrap_or_else(|| suite_name.clone()),
            version: suite_version.clone(),
            path: suite_path.display().to_string(),
            filter: scenario_filter.clone(),
            task_count,
        },
        provenance: ScenarioProvenance {
            suite_sha256,
            git_commit,
            git_branch,
        },
        harness: HarnessRunInfo {
            command: std::iter::once("python3".to_string())
                .chain(harness_args.iter().cloned())
                .collect(),
            exit_code: output.status.code().unwrap_or(1),
            gate_ok: harness_gate_ok,
            overall_score: harness_overall_score,
            gate_reasons: harness_reasons,
        },
        baseline: BaselineComparison {
            required: baseline_required,
            path: baseline_path.display().to_string(),
            baseline_found: baseline_metrics.is_some(),
            baseline_version: baseline_metrics.as_ref().and_then(|b| b.version.clone()),
            baseline_gate_ok: baseline_metrics.as_ref().and_then(|b| b.gate_ok),
            baseline_overall_score: baseline_metrics.as_ref().and_then(|b| b.overall_score),
            score_delta,
            max_regression: opts.max_regression,
            passed: baseline_ok,
            reasons: baseline_reasons,
        },
        gate: GateSummary {
            passed,
            reasons: gate_reasons,
        },
        artifacts: ArtifactPaths {
            report_json,
            report_md,
            history_jsonl,
            normalized_json: normalized_path.display().to_string(),
        },
    };

    std::fs::write(
        &normalized_path,
        serde_json::to_string_pretty(&artifact).context("Failed to serialize eval artifact")?,
    )
    .with_context(|| format!("Failed to write {}", normalized_path.display()))?;

    if opts.update_baseline && passed {
        if let Some(parent) = baseline_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        std::fs::copy(&normalized_path, &baseline_path).with_context(|| {
            format!(
                "Failed to update baseline {} from {}",
                baseline_path.display(),
                normalized_path.display()
            )
        })?;
    }

    persist_eval_kpi_record(
        config,
        &run_id,
        &suite_name,
        &suite_version,
        &suite_path,
        &baseline_path,
        baseline_metrics.as_ref(),
        harness_overall_score,
        score_delta,
        baseline_ok,
        passed,
        artifact.gate.reasons.clone(),
    );

    match opts.output_format {
        EvalOutputFormat::Text => {
            println!("eval_run_id={}", artifact.run_id);
            println!(
                "suite={} version={} tasks={} gate={} baseline={}",
                artifact.scenario.name,
                artifact.scenario.version,
                artifact.scenario.task_count,
                if artifact.harness.gate_ok { "PASS" } else { "FAIL" },
                if artifact.baseline.passed { "PASS" } else { "FAIL" }
            );
            if let Some(score) = artifact.harness.overall_score {
                println!("overall_score={:.3}", score);
            }
            if let Some(delta) = artifact.baseline.score_delta {
                println!("overall_delta={:.3}", delta);
            }
            println!("normalized_json={}", artifact.artifacts.normalized_json);
            if opts.update_baseline && passed {
                println!("baseline_updated={}", baseline_path.display());
            }
            if !artifact.gate.reasons.is_empty() {
                println!("gate_reasons:");
                for reason in &artifact.gate.reasons {
                    println!("- {}", reason);
                }
            }
        }
        EvalOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&artifact)
                    .context("Failed to print eval artifact as JSON")?
            );
        }
    }

    if !passed {
        anyhow::bail!(
            "eval failed: {}",
            if artifact.gate.reasons.is_empty() {
                "unknown reason".to_string()
            } else {
                artifact.gate.reasons.join("; ")
            }
        );
    }

    Ok(())
}

fn apply_scenario_filter(suite_json: &mut Value, filter: &str) -> anyhow::Result<()> {
    let Some(tasks) = suite_json.get_mut("tasks").and_then(Value::as_array_mut) else {
        anyhow::bail!("Suite JSON missing 'tasks' array")
    };
    tasks.retain(|task| {
        task.get("id")
            .and_then(Value::as_str)
            .map(|id| id.contains(filter))
            .unwrap_or(false)
    });
    if tasks.is_empty() {
        anyhow::bail!("No tasks matched filter '{}'", filter);
    }
    Ok(())
}

fn resolve_path(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    repo_root.join(path)
}

fn resolve_config_path(repo_root: &Path, cli_config_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = cli_config_path {
        let resolved = resolve_path(repo_root, path);
        if !resolved.exists() {
            anyhow::bail!("Config not found: {}", resolved.display());
        }
        return Ok(resolved);
    }

    let fallback = repo_root.join("config.toml");
    if fallback.exists() {
        Ok(fallback)
    } else {
        anyhow::bail!(
            "Config file not found. Pass --config <path> so eval harness can resolve DB and runtime settings."
        )
    }
}

fn parse_kv_path(stdout: &str, prefix: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        line.strip_prefix(prefix)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn parse_gate_ok(stdout: &str) -> Option<bool> {
    stdout.lines().find_map(|line| {
        line.strip_prefix("gate=")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| match v {
                "PASS" => Some(true),
                "FAIL" => Some(false),
                _ => None,
            })
    })
}

fn parse_overall_score(stdout: &str) -> Option<f64> {
    stdout.lines().find_map(|line| {
        line.split_whitespace().find_map(|tok| {
            tok.strip_prefix("overall=")
                .and_then(|raw| raw.parse::<f64>().ok())
        })
    })
}

fn load_harness_report(path: &Path) -> anyhow::Result<HarnessReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read harness report {}", path.display()))?;
    let report: HarnessReport = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse harness report {}", path.display()))?;
    Ok(report)
}

fn load_baseline_metrics(path: &Path) -> anyhow::Result<BaselineMetrics> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read baseline {}", path.display()))?;
    let v: Value = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse baseline {}", path.display()))?;

    if let Some(scenario) = v.get("scenario") {
        return Ok(BaselineMetrics {
            version: scenario
                .get("version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            gate_ok: v
                .get("gate")
                .and_then(|g| g.get("passed"))
                .and_then(Value::as_bool),
            overall_score: v
                .get("harness")
                .and_then(|h| h.get("overall_score"))
                .and_then(Value::as_f64),
        });
    }

    Ok(BaselineMetrics {
        version: v
            .get("version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        gate_ok: v.get("gate_ok").and_then(Value::as_bool),
        overall_score: v.get("overall_score").and_then(Value::as_f64),
    })
}

fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn git_provenance(repo_root: &Path) -> (Option<String>, Option<String>) {
    (
        git_cmd(repo_root, ["rev-parse", "HEAD"]),
        git_cmd(repo_root, ["rev-parse", "--abbrev-ref", "HEAD"]),
    )
}

fn git_cmd<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    let out = Command::new(OsStr::new("git"))
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[allow(clippy::too_many_arguments, reason = "Keeps call site explicit")]
fn persist_eval_kpi_record(
    config: &Config,
    run_id: &str,
    suite_name: &str,
    suite_version: &str,
    suite_path: &Path,
    baseline_path: &Path,
    baseline: Option<&BaselineMetrics>,
    overall_score: Option<f64>,
    score_delta: Option<f64>,
    baseline_ok: bool,
    gate_ok: bool,
    reasons: Vec<String>,
) {
    let conn = match kpi::open_connection(config) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to open DB for eval KPI persistence");
            return;
        }
    };

    let record = kpi::EvalRunRecord {
        run_id: run_id.to_string(),
        suite_name: suite_name.to_string(),
        suite_version: suite_version.to_string(),
        suite_path: suite_path.display().to_string(),
        baseline_path: Some(baseline_path.display().to_string()),
        baseline_version: baseline.and_then(|b| b.version.clone()),
        gate_ok,
        baseline_ok,
        overall_score,
        baseline_score: baseline.and_then(|b| b.overall_score),
        score_delta,
        reasons,
    };

    if let Err(e) = kpi::record_eval_run(&conn, &record) {
        tracing::warn!(error = %e, "Failed to persist eval KPI record");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gate_helpers_work() {
        let out = "report_json=eval/results/eval-1.json\ngate=FAIL overall=0.62\n";
        assert_eq!(parse_kv_path(out, "report_json=").as_deref(), Some("eval/results/eval-1.json"));
        assert_eq!(parse_gate_ok(out), Some(false));
        assert_eq!(parse_overall_score(out), Some(0.62));
    }

    #[test]
    fn sanitize_filename_replaces_special_chars() {
        assert_eq!(sanitize_filename("athena core/v2"), "athena-core-v2");
    }
}
