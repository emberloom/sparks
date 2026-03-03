use anyhow::Context;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCHEMA_VERSION_V1: &str = "v1";
const DEFAULT_SUITE_PATH: &str = "eval/benchmark-suite.json";
const DEFAULT_CONFIG_PATH: &str = "config.toml";
const DEFAULT_SCENARIO_LIBRARY_PATH: &str = "eval/scenario-library-v1.json";
const DEFAULT_SCORECARD_OUTPUT_PATH: &str = "eval/results/eval-scorecard-latest.json";
const DEFAULT_COMPARE_OUTPUT_PATH: &str = "eval/results/eval-compare-latest.json";

#[derive(Debug, Clone, Subcommand)]
pub enum EvalAction {
    /// Run an evaluation suite and emit a versioned JSON scorecard
    Run(EvalRunArgs),
    /// Compare two scorecards and emit a machine-readable regression report
    Compare(EvalCompareArgs),
}

#[derive(Debug, Clone, Args)]
pub struct EvalRunArgs {
    /// Benchmark suite JSON path
    #[arg(long, default_value = DEFAULT_SUITE_PATH)]
    pub suite: PathBuf,
    /// Config path override (falls back to global --config, then config.toml)
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Athena binary path override (defaults to currently running athena binary)
    #[arg(long)]
    pub athena_bin: Option<PathBuf>,
    /// Output scorecard JSON path
    #[arg(long, default_value = DEFAULT_SCORECARD_OUTPUT_PATH)]
    pub output: PathBuf,
    /// Versioned scenario library manifest
    #[arg(long, default_value = DEFAULT_SCENARIO_LIBRARY_PATH)]
    pub scenario_library: PathBuf,
    /// Deterministic seed tag passed to harness context/env
    #[arg(long)]
    pub seed: Option<u64>,
    /// Optional ATHENA_CLI_TIMEOUT_SECS passthrough to harness
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// Stop on first below-threshold task
    #[arg(long)]
    pub fail_fast: bool,
}

#[derive(Debug, Clone, Args)]
pub struct EvalCompareArgs {
    /// Baseline scorecard path
    #[arg(long)]
    pub baseline: PathBuf,
    /// Candidate scorecard path
    #[arg(long)]
    pub candidate: PathBuf,
    /// Output compare report JSON path
    #[arg(long, default_value = DEFAULT_COMPARE_OUTPUT_PATH)]
    pub output: PathBuf,
    /// Allowed drop for success metrics (higher is better)
    #[arg(long, default_value_t = 0.0)]
    pub max_success_drop: f64,
    /// Allowed drop for quality metrics (higher is better)
    #[arg(long, default_value_t = 0.0)]
    pub max_quality_drop: f64,
    /// Allowed increase for safety risk metrics (lower is better)
    #[arg(long, default_value_t = 0.0)]
    pub max_safety_increase: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioLibraryV1 {
    pub schema_version: String,
    pub library_id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub scenarios: Vec<ScenarioDefinitionV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioDefinitionV1 {
    pub id: String,
    pub suite_path: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunScorecardV1 {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub run: EvalRunMetadataV1,
    pub summary: EvalRunSummaryV1,
    pub dimensions: EvalRunDimensionsV1,
    pub tasks: Vec<EvalTaskScoreV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunMetadataV1 {
    pub scenario_library_path: String,
    pub scenario_library_id: String,
    pub scenario_id: String,
    pub suite_path: String,
    pub suite_name: String,
    pub config_path: String,
    pub config_sha256: Option<String>,
    pub athena_bin: String,
    pub git_commit_sha: Option<String>,
    pub harness_report_json: String,
    pub harness_exit_code: i32,
    pub seed: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub fail_fast: bool,
    pub harness_stdout_tail: String,
    pub harness_stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunSummaryV1 {
    pub verdict: String,
    pub gate_ok: bool,
    pub threshold: f64,
    pub overall_score: f64,
    pub task_count: usize,
    pub gate_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunDimensionsV1 {
    pub success: EvalSuccessDimensionV1,
    pub quality: EvalQualityDimensionV1,
    pub safety: EvalSafetyDimensionV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSuccessDimensionV1 {
    pub gate_ok: bool,
    pub succeeded_tasks: usize,
    pub total_tasks: usize,
    pub pass_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalQualityDimensionV1 {
    pub overall_score: f64,
    pub plan_quality_avg: f64,
    pub tests_pass_avg: f64,
    pub diff_quality_avg: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSafetyDimensionV1 {
    pub failed_tasks: usize,
    pub rolled_back_tasks: usize,
    pub timeout_tasks: usize,
    pub failed_rate: f64,
    pub rolled_back_rate: f64,
    pub timeout_rate: f64,
    pub gate_reasons_count: usize,
    pub gate_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalTaskScoreV1 {
    pub task_id: String,
    pub lane: String,
    pub risk: String,
    pub status: String,
    pub error: Option<String>,
    pub exec_success: f64,
    pub plan_quality: f64,
    pub tests_pass: f64,
    pub diff_quality: f64,
    pub overall: f64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCompareReportV1 {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub baseline_path: String,
    pub candidate_path: String,
    pub thresholds: EvalCompareThresholdsV1,
    pub verdict: String,
    pub regression_count: usize,
    pub regressions: Vec<String>,
    pub metrics: Vec<EvalMetricDeltaV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCompareThresholdsV1 {
    pub max_success_drop: f64,
    pub max_quality_drop: f64,
    pub max_safety_increase: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalMetricDeltaV1 {
    pub name: String,
    pub direction: String,
    pub threshold: f64,
    pub baseline: f64,
    pub candidate: f64,
    pub delta: f64,
    pub regression: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct HarnessReport {
    #[serde(default)]
    suite: String,
    #[serde(default)]
    threshold: f64,
    #[serde(default)]
    gate_ok: bool,
    #[serde(default)]
    gate_reasons: Vec<String>,
    #[serde(default)]
    overall_score: f64,
    #[serde(default)]
    results: Vec<HarnessTaskResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct HarnessTaskResult {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    lane: String,
    #[serde(default)]
    risk: String,
    #[serde(default)]
    status: String,
    error: Option<String>,
    #[serde(default)]
    exec_success: f64,
    #[serde(default)]
    plan_quality: f64,
    #[serde(default)]
    tests_pass: f64,
    #[serde(default)]
    diff_quality: f64,
    #[serde(default)]
    overall: f64,
    #[serde(default)]
    notes: Vec<String>,
}

pub fn handle_eval(action: EvalAction, global_config_path: Option<&Path>) -> anyhow::Result<()> {
    match action {
        EvalAction::Run(args) => handle_eval_run(args, global_config_path),
        EvalAction::Compare(args) => handle_eval_compare(args),
    }
}

fn handle_eval_run(args: EvalRunArgs, global_config_path: Option<&Path>) -> anyhow::Result<()> {
    let repo_root = resolve_repo_root()?;
    let cwd = std::env::current_dir().context("failed to get current dir")?;
    let suite_path = resolve_path(&repo_root, &args.suite);
    let config_path = resolve_path(
        &repo_root,
        args.config
            .as_deref()
            .or(global_config_path)
            .unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH)),
    );
    let athena_bin = match args.athena_bin {
        Some(path) => resolve_path(&repo_root, &path),
        None => std::env::current_exe().context("failed to resolve current athena binary")?,
    };
    let scorecard_output = resolve_path(&cwd, &args.output);
    let scenario_library_path = resolve_path(&repo_root, &args.scenario_library);
    let harness_script_path = repo_root.join("scripts/eval_harness.py");

    ensure_exists(&suite_path, "suite")?;
    ensure_exists(&config_path, "config")?;
    ensure_exists(&athena_bin, "athena binary")?;
    ensure_exists(&scenario_library_path, "scenario library")?;
    ensure_exists(&harness_script_path, "eval harness script")?;

    let scenario_library = load_scenario_library(&scenario_library_path)?;
    validate_schema_v1(&scenario_library.schema_version, "scenario library")?;

    let temp_run_dir = std::env::temp_dir().join(format!("athena-eval-run-{}", uuid::Uuid::new_v4()));
    let harness_output_dir = temp_run_dir.join("harness");
    let harness_history_path = temp_run_dir.join("history.jsonl");
    fs::create_dir_all(&harness_output_dir).with_context(|| {
        format!(
            "failed to create temporary harness output directory: {}",
            harness_output_dir.display()
        )
    })?;

    let mut command = Command::new("python3");
    command.arg(&harness_script_path);
    command.args(["--suite", suite_path.to_string_lossy().as_ref()]);
    command.args(["--config", config_path.to_string_lossy().as_ref()]);
    command.args(["--athena-bin", athena_bin.to_string_lossy().as_ref()]);
    command.args([
        "--output-dir",
        harness_output_dir.to_string_lossy().as_ref(),
    ]);
    command.args([
        "--history-file",
        harness_history_path.to_string_lossy().as_ref(),
    ]);
    if args.fail_fast {
        command.arg("--fail-fast");
    }
    if let Some(timeout_secs) = args.timeout_secs {
        command.args(["--cli-timeout-secs", timeout_secs.to_string().as_str()]);
    }
    if let Some(seed) = args.seed {
        command.args([
            "--dispatch-context",
            format!("[benchmark_seed:{}]", seed).as_str(),
        ]);
        command.env("ATHENA_EVAL_SEED", seed.to_string());
        command.env("PYTHONHASHSEED", seed.to_string());
    }

    let output = command
        .current_dir(&repo_root)
        .output()
        .context("failed to run scripts/eval_harness.py")?;
    let harness_exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let report_json_path = extract_report_json_path(&stdout, &repo_root)
        .or_else(|| extract_report_json_path(&stderr, &repo_root))
        .or_else(|| newest_matching_file(&harness_output_dir, "eval-", "json"))
        .with_context(|| {
            format!(
                "harness did not produce a report_json path (exit_code={}): {}",
                harness_exit_code,
                tail_text(&format!("{}\n{}", stdout, stderr), 1000)
            )
        })?;

    ensure_exists(&report_json_path, "harness report json")?;
    let harness_report: HarnessReport = serde_json::from_str(
        &fs::read_to_string(&report_json_path)
            .with_context(|| format!("failed to read harness report {}", report_json_path.display()))?,
    )
    .with_context(|| format!("failed to parse harness report {}", report_json_path.display()))?;

    let scenario = resolve_scenario(&scenario_library, &suite_path, &repo_root);
    let mut gate_reasons = harness_report.gate_reasons.clone();
    gate_reasons.sort();
    gate_reasons.dedup();

    let mut tasks: Vec<EvalTaskScoreV1> = harness_report
        .results
        .iter()
        .map(|r| EvalTaskScoreV1 {
            task_id: r.task_id.clone(),
            lane: r.lane.clone(),
            risk: r.risk.clone(),
            status: r.status.clone(),
            error: r.error.clone(),
            exec_success: r.exec_success,
            plan_quality: r.plan_quality,
            tests_pass: r.tests_pass,
            diff_quality: r.diff_quality,
            overall: r.overall,
            notes: r.notes.clone(),
        })
        .collect();
    tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));

    let total_tasks = tasks.len();
    let succeeded_tasks = tasks.iter().filter(|t| t.status == "succeeded").count();
    let failed_tasks = tasks.iter().filter(|t| t.status == "failed").count();
    let rolled_back_tasks = tasks.iter().filter(|t| t.status == "rolled_back").count();
    let timeout_tasks = tasks.iter().filter(|t| task_has_timeout(t)).count();
    let pass_rate = ratio(succeeded_tasks, total_tasks);
    let failed_rate = ratio(failed_tasks, total_tasks);
    let rolled_back_rate = ratio(rolled_back_tasks, total_tasks);
    let timeout_rate = ratio(timeout_tasks, total_tasks);

    let quality_overall = if harness_report.overall_score > 0.0 {
        harness_report.overall_score
    } else {
        avg(tasks.iter().map(|t| t.overall))
    };
    let plan_quality_avg = avg(tasks.iter().map(|t| t.plan_quality));
    let tests_pass_avg = avg(tasks.iter().map(|t| t.tests_pass));
    let diff_quality_avg = avg(tasks.iter().map(|t| t.diff_quality));

    let scorecard = EvalRunScorecardV1 {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
        run: EvalRunMetadataV1 {
            scenario_library_path: scenario_library_path.to_string_lossy().to_string(),
            scenario_library_id: scenario_library.library_id,
            scenario_id: scenario.id,
            suite_path: suite_path.to_string_lossy().to_string(),
            suite_name: harness_report.suite,
            config_path: config_path.to_string_lossy().to_string(),
            config_sha256: compute_sha256_hex(&config_path)?,
            athena_bin: athena_bin.to_string_lossy().to_string(),
            git_commit_sha: git_commit_sha(&repo_root),
            harness_report_json: report_json_path.to_string_lossy().to_string(),
            harness_exit_code,
            seed: args.seed,
            timeout_secs: args.timeout_secs,
            fail_fast: args.fail_fast,
            harness_stdout_tail: tail_text(&stdout, 1200),
            harness_stderr_tail: tail_text(&stderr, 1200),
        },
        summary: EvalRunSummaryV1 {
            verdict: if harness_report.gate_ok {
                "ok".to_string()
            } else {
                "regression".to_string()
            },
            gate_ok: harness_report.gate_ok,
            threshold: harness_report.threshold,
            overall_score: quality_overall,
            task_count: total_tasks,
            gate_reasons: gate_reasons.clone(),
        },
        dimensions: EvalRunDimensionsV1 {
            success: EvalSuccessDimensionV1 {
                gate_ok: harness_report.gate_ok,
                succeeded_tasks,
                total_tasks,
                pass_rate,
            },
            quality: EvalQualityDimensionV1 {
                overall_score: quality_overall,
                plan_quality_avg,
                tests_pass_avg,
                diff_quality_avg,
            },
            safety: EvalSafetyDimensionV1 {
                failed_tasks,
                rolled_back_tasks,
                timeout_tasks,
                failed_rate,
                rolled_back_rate,
                timeout_rate,
                gate_reasons_count: gate_reasons.len(),
                gate_reasons,
            },
        },
        tasks,
    };

    if let Some(parent) = scorecard_output.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create scorecard output directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(
        &scorecard_output,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&scorecard).context("failed to serialize scorecard")?
        ),
    )
    .with_context(|| format!("failed to write scorecard {}", scorecard_output.display()))?;

    println!("scorecard_json={}", scorecard_output.display());
    println!("harness_report_json={}", report_json_path.display());
    println!("harness_exit_code={}", harness_exit_code);
    println!(
        "verdict={}",
        if scorecard.summary.gate_ok { "ok" } else { "regression" }
    );

    let _ = fs::remove_dir_all(&temp_run_dir);
    Ok(())
}

fn handle_eval_compare(args: EvalCompareArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to get current dir")?;
    let baseline_path = resolve_path(&cwd, &args.baseline);
    let candidate_path = resolve_path(&cwd, &args.candidate);
    let output_path = resolve_path(&cwd, &args.output);

    ensure_exists(&baseline_path, "baseline scorecard")?;
    ensure_exists(&candidate_path, "candidate scorecard")?;

    let baseline: EvalRunScorecardV1 = serde_json::from_str(
        &fs::read_to_string(&baseline_path)
            .with_context(|| format!("failed to read baseline {}", baseline_path.display()))?,
    )
    .with_context(|| format!("failed to parse baseline {}", baseline_path.display()))?;
    let candidate: EvalRunScorecardV1 = serde_json::from_str(
        &fs::read_to_string(&candidate_path)
            .with_context(|| format!("failed to read candidate {}", candidate_path.display()))?,
    )
    .with_context(|| format!("failed to parse candidate {}", candidate_path.display()))?;

    validate_schema_v1(&baseline.schema_version, "baseline scorecard")?;
    validate_schema_v1(&candidate.schema_version, "candidate scorecard")?;

    let thresholds = EvalCompareThresholdsV1 {
        max_success_drop: args.max_success_drop,
        max_quality_drop: args.max_quality_drop,
        max_safety_increase: args.max_safety_increase,
    };
    let report = compare_scorecards(&baseline, &candidate, &thresholds, &baseline_path, &candidate_path);

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create compare output directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(
        &output_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&report).context("failed to serialize compare report")?
        ),
    )
    .with_context(|| format!("failed to write compare report {}", output_path.display()))?;

    println!("compare_report_json={}", output_path.display());
    println!("verdict={}", report.verdict);
    println!("regression_count={}", report.regression_count);
    Ok(())
}

fn compare_scorecards(
    baseline: &EvalRunScorecardV1,
    candidate: &EvalRunScorecardV1,
    thresholds: &EvalCompareThresholdsV1,
    baseline_path: &Path,
    candidate_path: &Path,
) -> EvalCompareReportV1 {
    let mut metrics = vec![
        metric_higher_is_better(
            "success.gate_ok",
            bool_to_score(baseline.dimensions.success.gate_ok),
            bool_to_score(candidate.dimensions.success.gate_ok),
            thresholds.max_success_drop,
        ),
        metric_higher_is_better(
            "success.pass_rate",
            baseline.dimensions.success.pass_rate,
            candidate.dimensions.success.pass_rate,
            thresholds.max_success_drop,
        ),
        metric_higher_is_better(
            "quality.overall_score",
            baseline.dimensions.quality.overall_score,
            candidate.dimensions.quality.overall_score,
            thresholds.max_quality_drop,
        ),
        metric_higher_is_better(
            "quality.plan_quality_avg",
            baseline.dimensions.quality.plan_quality_avg,
            candidate.dimensions.quality.plan_quality_avg,
            thresholds.max_quality_drop,
        ),
        metric_higher_is_better(
            "quality.tests_pass_avg",
            baseline.dimensions.quality.tests_pass_avg,
            candidate.dimensions.quality.tests_pass_avg,
            thresholds.max_quality_drop,
        ),
        metric_higher_is_better(
            "quality.diff_quality_avg",
            baseline.dimensions.quality.diff_quality_avg,
            candidate.dimensions.quality.diff_quality_avg,
            thresholds.max_quality_drop,
        ),
        metric_lower_is_better(
            "safety.failed_rate",
            baseline.dimensions.safety.failed_rate,
            candidate.dimensions.safety.failed_rate,
            thresholds.max_safety_increase,
        ),
        metric_lower_is_better(
            "safety.rolled_back_rate",
            baseline.dimensions.safety.rolled_back_rate,
            candidate.dimensions.safety.rolled_back_rate,
            thresholds.max_safety_increase,
        ),
        metric_lower_is_better(
            "safety.timeout_rate",
            baseline.dimensions.safety.timeout_rate,
            candidate.dimensions.safety.timeout_rate,
            thresholds.max_safety_increase,
        ),
        metric_lower_is_better(
            "safety.gate_reasons_count",
            baseline.dimensions.safety.gate_reasons_count as f64,
            candidate.dimensions.safety.gate_reasons_count as f64,
            thresholds.max_safety_increase,
        ),
    ];
    metrics.sort_by(|a, b| a.name.cmp(&b.name));

    let mut regressions: Vec<String> = metrics
        .iter()
        .filter(|m| m.regression)
        .map(|m| m.name.clone())
        .collect();
    regressions.sort();
    regressions.dedup();

    EvalCompareReportV1 {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
        baseline_path: baseline_path.to_string_lossy().to_string(),
        candidate_path: candidate_path.to_string_lossy().to_string(),
        thresholds: thresholds.clone(),
        verdict: if regressions.is_empty() {
            "ok".to_string()
        } else {
            "regression".to_string()
        },
        regression_count: regressions.len(),
        regressions,
        metrics,
    }
}

fn metric_higher_is_better(
    name: &str,
    baseline: f64,
    candidate: f64,
    max_drop: f64,
) -> EvalMetricDeltaV1 {
    let delta = candidate - baseline;
    EvalMetricDeltaV1 {
        name: name.to_string(),
        direction: "higher_is_better".to_string(),
        threshold: max_drop,
        baseline,
        candidate,
        delta,
        regression: delta < -max_drop,
    }
}

fn metric_lower_is_better(
    name: &str,
    baseline: f64,
    candidate: f64,
    max_increase: f64,
) -> EvalMetricDeltaV1 {
    let delta = candidate - baseline;
    EvalMetricDeltaV1 {
        name: name.to_string(),
        direction: "lower_is_better".to_string(),
        threshold: max_increase,
        baseline,
        candidate,
        delta,
        regression: delta > max_increase,
    }
}

fn bool_to_score(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

fn avg<I>(iter: I) -> f64
where
    I: Iterator<Item = f64>,
{
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for value in iter {
        sum += value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn task_has_timeout(task: &EvalTaskScoreV1) -> bool {
    if task
        .error
        .as_deref()
        .map(|e| e.to_ascii_lowercase().contains("timeout"))
        .unwrap_or(false)
    {
        return true;
    }
    task.notes.iter().any(|n| {
        let lower = n.to_ascii_lowercase();
        lower.contains("timeout") || lower.contains("outcome_not_terminal_after")
    })
}

fn load_scenario_library(path: &Path) -> anyhow::Result<ScenarioLibraryV1> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read scenario library {}", path.display()))?;
    let library: ScenarioLibraryV1 = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse scenario library {}", path.display()))?;
    Ok(library)
}

struct ResolvedScenario {
    id: String,
}

fn resolve_scenario(
    library: &ScenarioLibraryV1,
    suite_path: &Path,
    repo_root: &Path,
) -> ResolvedScenario {
    let normalized_suite = normalize_path(suite_path);
    for scenario in &library.scenarios {
        let candidate = resolve_path(repo_root, Path::new(&scenario.suite_path));
        if normalize_path(&candidate) == normalized_suite {
            return ResolvedScenario {
                id: scenario.id.clone(),
            };
        }
    }
    let fallback = suite_path
        .file_stem()
        .and_then(OsStr::to_str)
        .map(str::to_string)
        .unwrap_or_else(|| "custom_suite".to_string());
    ResolvedScenario { id: fallback }
}

fn extract_report_json_path(text: &str, repo_root: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        if let Some(raw) = line.trim().strip_prefix("report_json=") {
            let candidate = PathBuf::from(raw.trim());
            return Some(resolve_path(repo_root, &candidate));
        }
    }
    None
}

fn newest_matching_file(dir: &Path, prefix: &str, extension: &str) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .map(|name| {
                    name.starts_with(prefix)
                        && path
                            .extension()
                            .and_then(OsStr::to_str)
                            .map(|ext| ext.eq_ignore_ascii_case(extension))
                            .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files.pop()
}

fn resolve_repo_root() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to resolve current dir")?;
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if root.is_empty() {
                Ok(cwd)
            } else {
                Ok(PathBuf::from(root))
            }
        }
        _ => Ok(cwd),
    }
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn compute_sha256_hex(path: &Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        fs::read(path).with_context(|| format!("failed to read file for hash {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn git_commit_sha(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn ensure_exists(path: &Path, label: &str) -> anyhow::Result<()> {
    if path.exists() {
        Ok(())
    } else {
        anyhow::bail!("{} not found: {}", label, path.display())
    }
}

fn validate_schema_v1(version: &str, label: &str) -> anyhow::Result<()> {
    if version == SCHEMA_VERSION_V1 {
        Ok(())
    } else {
        anyhow::bail!(
            "{} schema_version must be '{}' but was '{}'",
            label,
            SCHEMA_VERSION_V1,
            version
        )
    }
}

fn tail_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = String::new();
    for c in input
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
    {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scorecard(
        pass_rate: f64,
        gate_ok: bool,
        quality_overall: f64,
        failed_rate: f64,
    ) -> EvalRunScorecardV1 {
        EvalRunScorecardV1 {
            schema_version: SCHEMA_VERSION_V1.to_string(),
            generated_at_utc: "2026-03-02T00:00:00Z".to_string(),
            run: EvalRunMetadataV1 {
                scenario_library_path: "eval/scenario-library-v1.json".to_string(),
                scenario_library_id: "athena-eval-scenarios".to_string(),
                scenario_id: "sample".to_string(),
                suite_path: "eval/benchmark-suite.json".to_string(),
                suite_name: "athena-core-v2-real".to_string(),
                config_path: "config.toml".to_string(),
                config_sha256: None,
                athena_bin: "target/debug/athena".to_string(),
                git_commit_sha: Some("abc".to_string()),
                harness_report_json: "eval/results/eval-raw.json".to_string(),
                harness_exit_code: 0,
                seed: None,
                timeout_secs: None,
                fail_fast: false,
                harness_stdout_tail: String::new(),
                harness_stderr_tail: String::new(),
            },
            summary: EvalRunSummaryV1 {
                verdict: if gate_ok {
                    "ok".to_string()
                } else {
                    "regression".to_string()
                },
                gate_ok,
                threshold: 0.7,
                overall_score: quality_overall,
                task_count: 2,
                gate_reasons: vec![],
            },
            dimensions: EvalRunDimensionsV1 {
                success: EvalSuccessDimensionV1 {
                    gate_ok,
                    succeeded_tasks: if pass_rate >= 1.0 { 2 } else { 1 },
                    total_tasks: 2,
                    pass_rate,
                },
                quality: EvalQualityDimensionV1 {
                    overall_score: quality_overall,
                    plan_quality_avg: quality_overall,
                    tests_pass_avg: quality_overall,
                    diff_quality_avg: quality_overall,
                },
                safety: EvalSafetyDimensionV1 {
                    failed_tasks: if failed_rate > 0.0 { 1 } else { 0 },
                    rolled_back_tasks: 0,
                    timeout_tasks: 0,
                    failed_rate,
                    rolled_back_rate: 0.0,
                    timeout_rate: 0.0,
                    gate_reasons_count: 0,
                    gate_reasons: vec![],
                },
            },
            tasks: vec![],
        }
    }

    #[test]
    fn compare_detects_regressions() {
        let baseline = sample_scorecard(1.0, true, 0.9, 0.0);
        let candidate = sample_scorecard(0.5, false, 0.6, 0.5);
        let thresholds = EvalCompareThresholdsV1 {
            max_success_drop: 0.0,
            max_quality_drop: 0.0,
            max_safety_increase: 0.0,
        };

        let report = compare_scorecards(
            &baseline,
            &candidate,
            &thresholds,
            Path::new("baseline.json"),
            Path::new("candidate.json"),
        );
        assert_eq!(report.verdict, "regression");
        assert!(report.regressions.contains(&"success.pass_rate".to_string()));
        assert!(report.regressions.contains(&"quality.overall_score".to_string()));
        assert!(report.regressions.contains(&"safety.failed_rate".to_string()));
    }

    #[test]
    fn compare_respects_thresholds() {
        let baseline = sample_scorecard(1.0, true, 0.9, 0.0);
        let candidate = sample_scorecard(0.95, true, 0.86, 0.02);
        let thresholds = EvalCompareThresholdsV1 {
            max_success_drop: 0.1,
            max_quality_drop: 0.1,
            max_safety_increase: 0.1,
        };

        let report = compare_scorecards(
            &baseline,
            &candidate,
            &thresholds,
            Path::new("baseline.json"),
            Path::new("candidate.json"),
        );
        assert_eq!(report.verdict, "ok");
        assert_eq!(report.regression_count, 0);
    }

    #[test]
    fn compare_metrics_are_sorted() {
        let baseline = sample_scorecard(1.0, true, 0.9, 0.0);
        let candidate = sample_scorecard(1.0, true, 0.9, 0.0);
        let thresholds = EvalCompareThresholdsV1 {
            max_success_drop: 0.0,
            max_quality_drop: 0.0,
            max_safety_increase: 0.0,
        };

        let report = compare_scorecards(
            &baseline,
            &candidate,
            &thresholds,
            Path::new("baseline.json"),
            Path::new("candidate.json"),
        );
        let mut names: Vec<String> = report.metrics.iter().map(|m| m.name.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        names.dedup();
        assert_eq!(names.len(), report.metrics.len());
    }
}
