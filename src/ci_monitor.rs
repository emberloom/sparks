use crate::observer::{ObserverCategory, ObserverHandle};
use crate::{
    args, command_combined_output, command_succeeded, parse_dispatch_task_id,
    read_task_outcome_status, resolve_child_dispatch_config_path, run_command_capture,
    tail_text, wait_for_terminal_outcome_status, CommandRunResult, Config,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::time::{Duration, Instant};

pub const CI_POLL_INTERVAL_SECS: u64 = 45;
pub const CI_POLL_TIMEOUT_SECS: u64 = 1200;
pub const CI_HEAL_MAX_ATTEMPTS: u8 = 2;

const CI_LOG_TAIL_CHARS: usize = 4000;
const CI_COMMAND_TIMEOUT_SECS: u64 = 120;
const CI_DISPATCH_WAIT_SECS: u64 = 300;
const CI_POST_MERGE_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiCheckStatus {
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiPollResult {
    pub timestamp_utc: String,
    pub overall: String,
    pub checks: Vec<CiCheckStatus>,
    pub raw_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiHealAttempt {
    pub attempt: u8,
    pub failure_logs: String,
    pub dispatch_task_id: Option<String>,
    pub dispatch_status: String,
    pub commit_sha: Option<String>,
    pub ci_result: Option<CiPollResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiMonitorCommand {
    pub name: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub status: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiMonitorReport {
    pub pr_url: String,
    pub branch: Option<String>,
    pub started_utc: String,
    pub finished_utc: String,
    pub final_status: String,
    #[serde(default = "default_post_merge_status")]
    pub post_merge_status: String,
    #[serde(default)]
    pub revert_pr_url: Option<String>,
    pub polls: Vec<CiPollResult>,
    pub heal_attempts: Vec<CiHealAttempt>,
    pub merged_after_ci: bool,
    pub commands: Vec<CiMonitorCommand>,
}

fn default_post_merge_status() -> String {
    "not_checked".to_string()
}

#[derive(Debug, Deserialize)]
struct StatusCheckResponse {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Vec<StatusCheckEntry>,
}

#[derive(Debug, Deserialize)]
struct StatusCheckEntry {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(rename = "detailsUrl")]
    details_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrHeadRefResponse {
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrCheckEntry {
    name: Option<String>,
    state: Option<String>,
    link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrMergeInfoResponse {
    number: Option<u64>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: Option<String>,
    #[serde(rename = "mergeCommit")]
    merge_commit: Option<PrMergeCommit>,
}

#[derive(Debug, Deserialize)]
struct PrMergeCommit {
    oid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitCheckRunsResponse {
    #[serde(default)]
    check_runs: Vec<CommitCheckRunEntry>,
}

#[derive(Debug, Deserialize)]
struct CommitCheckRunEntry {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(rename = "html_url")]
    html_url: Option<String>,
}

struct CiMonitorContext {
    commands: Vec<CiMonitorCommand>,
    observer: Option<ObserverHandle>,
}

impl CiMonitorContext {
    fn new(spawn_observer: bool) -> Self {
        let observer = if spawn_observer {
            let handle = ObserverHandle::new(256);
            crate::observer::spawn_uds_listener(handle.clone());
            Some(handle)
        } else {
            None
        };
        Self {
            commands: Vec::new(),
            observer,
        }
    }

    fn record(&mut self, name: &str, run: &CommandRunResult) {
        self.commands
            .push(build_ci_monitor_command(name, run));
    }

    fn log(&self, message: impl Into<String>) {
        if let Some(observer) = &self.observer {
            observer.log(ObserverCategory::CiMonitor, message);
        }
    }
}

pub async fn monitor_pr_ci(
    pr_url: &str,
    branch: Option<&str>,
    repo_root: &Path,
    config: &Config,
    auto_merge: bool,
    heal: bool,
    poll_interval: u64,
    timeout: u64,
    max_heal: u8,
) -> CiMonitorReport {
    let started_utc = chrono::Utc::now().to_rfc3339();
    let mut ctx = CiMonitorContext::new(true);
    ctx.log(format!("ci monitor started pr={}", pr_url));

    let mut branch_name = branch.map(|b| b.to_string());
    if heal && branch_name.is_none() {
        branch_name = resolve_pr_branch(pr_url, repo_root, &mut ctx).await;
    }

    let mut polls = Vec::new();
    let mut heal_attempts = Vec::new();
    let start = Instant::now();
    let poll_wait = poll_interval.max(5);

    let final_status = monitor_poll_loop(
        pr_url, repo_root, config, heal, max_heal, timeout, poll_wait,
        &branch_name, &mut polls, &mut heal_attempts, &start, &mut ctx,
    )
    .await;

    let merged_after_ci = if auto_merge && (final_status == "ci_passed" || final_status == "heal_succeeded") {
        try_auto_merge(pr_url, repo_root, &mut ctx).await
    } else {
        false
    };

    let (post_merge_status, revert_pr_url) =
        monitor_post_merge_health(pr_url, repo_root, poll_wait, &mut ctx).await;

    let finished_utc = chrono::Utc::now().to_rfc3339();
    CiMonitorReport {
        pr_url: pr_url.to_string(),
        branch: branch_name,
        started_utc,
        finished_utc,
        final_status,
        post_merge_status,
        revert_pr_url,
        polls,
        heal_attempts,
        merged_after_ci,
        commands: ctx.commands,
    }
}

#[allow(clippy::too_many_arguments)]
async fn monitor_poll_loop(
    pr_url: &str,
    repo_root: &Path,
    config: &Config,
    heal: bool,
    max_heal: u8,
    timeout: u64,
    poll_wait: u64,
    branch_name: &Option<String>,
    polls: &mut Vec<CiPollResult>,
    heal_attempts: &mut Vec<CiHealAttempt>,
    start: &Instant,
    ctx: &mut CiMonitorContext,
) -> String {
    loop {
        if start.elapsed() >= Duration::from_secs(timeout) {
            ctx.log("ci monitor timeout");
            return "timeout".to_string();
        }

        let poll = poll_pr_ci_status_internal(pr_url, repo_root, ctx).await;
        let overall = poll.overall.clone();
        polls.push(poll.clone());

        match overall.as_str() {
            "passing" => {
                let s = if heal_attempts.is_empty() { "ci_passed" } else { "heal_succeeded" };
                ctx.log(format!("ci monitor success status={}", s));
                return s.to_string();
            }
            "failing" => {
                if let Some(status) = handle_failing_poll(
                    repo_root, config, heal, max_heal, branch_name,
                    pr_url, &poll, heal_attempts, ctx,
                ).await {
                    return status;
                }
                tokio::time::sleep(Duration::from_secs(poll_wait)).await;
            }
            _ => {
                ctx.log("ci monitor pending checks");
                tokio::time::sleep(Duration::from_secs(poll_wait)).await;
            }
        }
    }
}

async fn handle_failing_poll(
    repo_root: &Path,
    config: &Config,
    heal: bool,
    max_heal: u8,
    branch_name: &Option<String>,
    pr_url: &str,
    poll: &CiPollResult,
    heal_attempts: &mut Vec<CiHealAttempt>,
    ctx: &mut CiMonitorContext,
) -> Option<String> {
    ctx.log("ci monitor detected failing checks");
    let can_heal = heal && max_heal > 0 && branch_name.is_some();
    if !can_heal {
        ctx.log("ci monitor failing without heal capability");
        return Some("ci_failed".to_string());
    }
    if heal_attempts.len() >= max_heal as usize {
        ctx.log("ci monitor heal attempts exhausted");
        return Some("heal_exhausted".to_string());
    }
    let attempt_num = heal_attempts.len() as u8 + 1;
    let failure_logs = extract_failed_ci_logs_internal(pr_url, repo_root, ctx).await;
    let mut attempt = heal_ci_failure_internal(
        repo_root, branch_name.as_deref().unwrap_or(""),
        &failure_logs, attempt_num, config, ctx,
    ).await;
    attempt.ci_result = Some(poll.clone());
    heal_attempts.push(attempt);
    ctx.log(format!("ci monitor heal attempt {} dispatched", attempt_num));
    None
}

async fn try_auto_merge(pr_url: &str, repo_root: &Path, ctx: &mut CiMonitorContext) -> bool {
    let merge_run = run_command_capture(
        repo_root,
        "gh",
        &[
            "pr".to_string(), "merge".to_string(), pr_url.to_string(),
            "--squash".to_string(), "--delete-branch".to_string(),
        ],
        240,
    ).await;
    ctx.record("gh_pr_merge", &merge_run);
    if command_succeeded(&merge_run) {
        ctx.log("ci monitor auto-merge succeeded");
        true
    } else {
        ctx.log("ci monitor auto-merge failed");
        false
    }
}

async fn poll_pr_ci_status_internal(
    pr_url: &str,
    workdir: &Path,
    ctx: &mut CiMonitorContext,
) -> CiPollResult {
    let run = run_command_capture(
        workdir,
        "gh",
        &[
            "pr".to_string(),
            "view".to_string(),
            pr_url.to_string(),
            "--json".to_string(),
            "statusCheckRollup".to_string(),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("gh_pr_view_checks", &run);

    let raw_json = if !run.stdout.trim().is_empty() {
        run.stdout.clone()
    } else {
        command_combined_output(&run)
    };
    let timestamp_utc = chrono::Utc::now().to_rfc3339();

    if !command_succeeded(&run) {
        return CiPollResult {
            timestamp_utc,
            overall: "pending".to_string(),
            checks: Vec::new(),
            raw_json,
        };
    }

    let checks = match parse_status_check_rollup(&raw_json) {
        Some(c) => c,
        None => {
            return CiPollResult {
                timestamp_utc,
                overall: "pending".to_string(),
                checks: Vec::new(),
                raw_json,
            };
        }
    };
    let overall = compute_overall(&checks);

    CiPollResult {
        timestamp_utc,
        overall,
        checks,
        raw_json,
    }
}

async fn extract_failed_ci_logs_internal(
    pr_url: &str,
    workdir: &Path,
    ctx: &mut CiMonitorContext,
) -> String {
    let run = run_command_capture(
        workdir,
        "gh",
        &[
            "pr".to_string(),
            "checks".to_string(),
            pr_url.to_string(),
            "--json".to_string(),
            "name,state,link".to_string(),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("gh_pr_checks", &run);

    let raw_json = if !run.stdout.trim().is_empty() {
        run.stdout.clone()
    } else {
        command_combined_output(&run)
    };

    let mut logs = String::new();
    let checks = parse_pr_checks(&raw_json);
    for check in checks {
        let state = check
            .state
            .as_deref()
            .unwrap_or_default()
            .to_uppercase();
        if state != "FAIL" && state != "FAILURE" && state != "ERROR" {
            continue;
        }
        let name = check.name.unwrap_or_else(|| "unknown".to_string());
        let run_id = check
            .link
            .as_deref()
            .and_then(extract_run_id);
        if let Some(run_id) = run_id {
            let run_view = run_command_capture(
                workdir,
                "gh",
                &[
                    "run".to_string(),
                    "view".to_string(),
                    run_id.clone(),
                    "--log-failed".to_string(),
                ],
                CI_COMMAND_TIMEOUT_SECS,
            )
            .await;
            ctx.record(&format!("gh_run_view_{}", run_id), &run_view);
            let out = command_combined_output(&run_view);
            logs.push_str(&format!("== {} ({}) ==\n", name, run_id));
            logs.push_str(&out);
            if !out.ends_with('\n') {
                logs.push('\n');
            }
        } else {
            logs.push_str(&format!("== {} ==\n", name));
            logs.push_str("missing run id for log fetch\n");
        }
    }

    tail_text(&logs, CI_LOG_TAIL_CHARS)
}


async fn heal_ci_failure_internal(
    repo_root: &Path,
    branch: &str,
    failure_logs: &str,
    attempt: u8,
    config: &Config,
    ctx: &mut CiMonitorContext,
) -> CiHealAttempt {
    let trimmed_logs = tail_text(failure_logs, CI_LOG_TAIL_CHARS);
    let mut rec = CiHealAttempt {
        attempt,
        failure_logs: trimmed_logs.clone(),
        dispatch_task_id: None,
        dispatch_status: "skipped".to_string(),
        commit_sha: None,
        ci_result: None,
    };

    if branch.trim().is_empty() {
        rec.dispatch_status = "failed: missing branch".to_string();
        return rec;
    }

    let worktree_path = match setup_heal_worktree(repo_root, branch, ctx).await {
        Ok(p) => p,
        Err(reason) => { rec.dispatch_status = reason; return rec; }
    };

    let result = run_heal_dispatch(&worktree_path, repo_root, branch, &trimmed_logs, attempt, config, ctx, &mut rec).await;
    cleanup_ci_worktree(repo_root, &worktree_path, ctx).await;
    rec.dispatch_status = result;
    rec
}

async fn setup_heal_worktree(
    repo_root: &Path,
    branch: &str,
    ctx: &mut CiMonitorContext,
) -> Result<std::path::PathBuf, String> {
    let fetch = run_command_capture(
        repo_root, "git", &args(&["fetch", "origin", branch]), CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_fetch", &fetch);
    if !command_succeeded(&fetch) {
        return Err("failed: git fetch".to_string());
    }

    let worktree_root = repo_root.join(".worktrees");
    let _ = std::fs::create_dir_all(&worktree_root);
    let safe_branch = sanitize_branch(branch);
    let worktree_path = worktree_root.join(format!("ci-heal-{}", safe_branch));
    let worktree_path_s = worktree_path.to_string_lossy().to_string();

    if worktree_path.exists() {
        let remove = run_command_capture(
            repo_root, "git",
            &args(&["worktree", "remove", "--force", &worktree_path_s]),
            CI_COMMAND_TIMEOUT_SECS,
        ).await;
        ctx.record("git_worktree_remove", &remove);
    }

    let add = run_command_capture(
        repo_root, "git",
        &args(&["worktree", "add", &worktree_path_s, &format!("origin/{}", branch)]),
        CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_worktree_add", &add);
    if !command_succeeded(&add) {
        return Err("failed: git worktree add".to_string());
    }
    Ok(worktree_path)
}

#[allow(clippy::too_many_arguments)]
async fn run_heal_dispatch(
    worktree_path: &Path,
    repo_root: &Path,
    branch: &str,
    trimmed_logs: &str,
    attempt: u8,
    config: &Config,
    ctx: &mut CiMonitorContext,
    rec: &mut CiHealAttempt,
) -> String {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return format!("failed: current_exe {}", e),
    };

    let mut dispatch_args = vec![
        "dispatch".to_string(), "--goal".to_string(), "Fix CI failure".to_string(),
        "--context".to_string(), trimmed_logs.to_string(),
        "--ghost".to_string(), "coder".to_string(),
        "--wait-secs".to_string(), CI_DISPATCH_WAIT_SECS.to_string(),
    ];
    if let Some(config_path) = resolve_child_dispatch_config_path(repo_root) {
        dispatch_args.insert(0, config_path.to_string_lossy().to_string());
        dispatch_args.insert(0, "--config".to_string());
    }

    let exe_s = exe.to_string_lossy().to_string();
    let dispatch_run = run_command_capture(
        worktree_path, &exe_s, &dispatch_args, CI_DISPATCH_WAIT_SECS.saturating_add(180),
    ).await;
    ctx.record("athena_dispatch_ci_heal", &dispatch_run);

    let dispatch_output = command_combined_output(&dispatch_run);
    let dispatch_task_id = parse_dispatch_task_id(&dispatch_output);
    rec.dispatch_task_id = dispatch_task_id.clone();

    let mut status = if dispatch_run.timed_out {
        "timeout".to_string()
    } else if dispatch_run.exit_code == Some(0) {
        "succeeded".to_string()
    } else {
        "failed".to_string()
    };
    if let Some(task_id) = dispatch_task_id.as_deref() {
        let _ = wait_for_terminal_outcome_status(config, task_id, 30).await;
        if let Ok(Some(s)) = read_task_outcome_status(config, task_id) {
            status = s;
        }
    }

    commit_and_push_heal(worktree_path, branch, attempt, ctx, rec, &mut status).await;
    status
}

async fn commit_and_push_heal(
    worktree_path: &Path,
    branch: &str,
    attempt: u8,
    ctx: &mut CiMonitorContext,
    rec: &mut CiHealAttempt,
    status: &mut String,
) {
    let status_run = run_command_capture(
        worktree_path, "git", &args(&["status", "--porcelain"]), CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_status", &status_run);
    if status_run.stdout.trim().is_empty() {
        status.push_str(" (no_changes)");
        return;
    }

    let add_run = run_command_capture(
        worktree_path, "git", &args(&["add", "-A"]), CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_add", &add_run);
    if !command_succeeded(&add_run) {
        status.push_str(" (add_failed)");
        return;
    }

    let commit_msg = format!("ci-heal: attempt {}", attempt);
    let commit_run = run_command_capture(
        worktree_path, "git",
        &["commit".to_string(), "-m".to_string(), commit_msg],
        CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_commit", &commit_run);
    if !command_succeeded(&commit_run) {
        status.push_str(" (commit_failed)");
        return;
    }

    let rev_run = run_command_capture(
        worktree_path, "git", &args(&["rev-parse", "HEAD"]), CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_rev_parse", &rev_run);
    if command_succeeded(&rev_run) {
        let sha = rev_run.stdout.trim();
        if !sha.is_empty() {
            rec.commit_sha = Some(sha.to_string());
        }
    }

    let push_run = run_command_capture(
        worktree_path, "git",
        &args(&["push", "origin", &format!("HEAD:{}", branch)]),
        CI_COMMAND_TIMEOUT_SECS,
    ).await;
    ctx.record("git_push", &push_run);
    if !command_succeeded(&push_run) {
        status.push_str(" (push_failed)");
    }
}

async fn cleanup_ci_worktree(repo_root: &Path, worktree_path: &Path, ctx: &mut CiMonitorContext) {
    let path_s = worktree_path.to_string_lossy().to_string();
    let remove = run_command_capture(
        repo_root,
        "git",
        &args(&["worktree", "remove", "--force", &path_s]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_worktree_remove", &remove);
    let prune = run_command_capture(repo_root, "git", &args(&["worktree", "prune"]), 60).await;
    ctx.record("git_worktree_prune", &prune);
}

async fn resolve_pr_branch(
    pr_url: &str,
    repo_root: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<String> {
    let run = run_command_capture(
        repo_root,
        "gh",
        &[
            "pr".to_string(),
            "view".to_string(),
            pr_url.to_string(),
            "--json".to_string(),
            "headRefName".to_string(),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("gh_pr_view_head", &run);
    if !command_succeeded(&run) {
        return None;
    }

    let raw_json = if !run.stdout.trim().is_empty() {
        run.stdout.clone()
    } else {
        command_combined_output(&run)
    };
    let parsed: PrHeadRefResponse = serde_json::from_str(raw_json.trim()).ok()?;
    parsed.head_ref_name.and_then(|b| {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Clone)]
struct ParsedPrUrl {
    owner: String,
    repo: String,
    number: u64,
}

#[derive(Debug, Clone)]
struct ResolvedPrMergeInfo {
    owner: String,
    repo: String,
    number: u64,
    base_branch: String,
    merged: bool,
    merge_commit_sha: Option<String>,
}

async fn monitor_post_merge_health(
    pr_url: &str,
    repo_root: &Path,
    poll_wait: u64,
    ctx: &mut CiMonitorContext,
) -> (String, Option<String>) {
    let Some(info) = resolve_pr_merge_info(pr_url, repo_root, ctx).await else {
        return ("post_merge_info_unavailable".to_string(), None);
    };
    if !info.merged {
        return ("not_merged".to_string(), None);
    }
    let Some(merge_sha) = info.merge_commit_sha.as_deref() else {
        return ("merge_commit_unavailable".to_string(), None);
    };

    let start = Instant::now();
    loop {
        if start.elapsed() >= Duration::from_secs(CI_POST_MERGE_TIMEOUT_SECS) {
            ctx.log("ci monitor post-merge timeout");
            return ("post_merge_timeout".to_string(), None);
        }

        let checks = poll_merge_commit_checks(&info.owner, &info.repo, merge_sha, repo_root, ctx).await;
        let overall = compute_post_merge_overall(checks.as_deref().unwrap_or(&[]));
        match overall.as_str() {
            "passing" => {
                ctx.log("ci monitor post-merge checks passing");
                return ("post_merge_passed".to_string(), None);
            }
            "failing" => {
                ctx.log("ci monitor post-merge checks failing; opening revert pr");
                let revert_pr_url = ensure_revert_pr_for_failed_merge(
                    &info.owner,
                    &info.repo,
                    info.number,
                    &info.base_branch,
                    merge_sha,
                    pr_url,
                    repo_root,
                    ctx,
                )
                .await;
                let status = if revert_pr_url.is_some() {
                    "post_merge_failed_revert_pr_opened"
                } else {
                    "post_merge_failed_revert_pr_error"
                };
                return (status.to_string(), revert_pr_url);
            }
            _ => {
                ctx.log("ci monitor post-merge checks pending");
                tokio::time::sleep(Duration::from_secs(poll_wait.max(5))).await;
            }
        }
    }
}

async fn resolve_pr_merge_info(
    pr_url: &str,
    repo_root: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<ResolvedPrMergeInfo> {
    let parsed_url = parse_pr_url(pr_url)?;
    let run = run_command_capture(
        repo_root,
        "gh",
        &[
            "pr".to_string(),
            "view".to_string(),
            pr_url.to_string(),
            "--json".to_string(),
            "number,mergedAt,baseRefName,mergeCommit".to_string(),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("gh_pr_view_merge_info", &run);
    if !command_succeeded(&run) {
        return None;
    }

    let raw_json = if !run.stdout.trim().is_empty() {
        run.stdout.clone()
    } else {
        command_combined_output(&run)
    };
    let response: PrMergeInfoResponse = serde_json::from_str(raw_json.trim()).ok()?;
    let merged = response
        .merged_at
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let merge_commit_sha = response
        .merge_commit
        .and_then(|c| c.oid)
        .and_then(|oid| {
            let trimmed = oid.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
    Some(ResolvedPrMergeInfo {
        owner: parsed_url.owner,
        repo: parsed_url.repo,
        number: response.number.unwrap_or(parsed_url.number),
        base_branch: response
            .base_ref_name
            .unwrap_or_else(|| "main".to_string()),
        merged,
        merge_commit_sha,
    })
}

async fn poll_merge_commit_checks(
    owner: &str,
    repo: &str,
    merge_sha: &str,
    repo_root: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<Vec<CiCheckStatus>> {
    let run = run_command_capture(
        repo_root,
        "gh",
        &[
            "api".to_string(),
            format!("repos/{}/{}/commits/{}/check-runs", owner, repo, merge_sha),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("gh_api_commit_check_runs", &run);
    if !command_succeeded(&run) {
        return None;
    }
    parse_commit_check_runs(run.stdout.trim())
}

async fn ensure_revert_pr_for_failed_merge(
    owner: &str,
    repo: &str,
    pr_number: u64,
    base_branch: &str,
    merge_sha: &str,
    source_pr_url: &str,
    repo_root: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<String> {
    let branch = ci_auto_revert_branch(merge_sha);
    if let Some(url) = lookup_revert_pr_url(owner, repo, &branch, "open", repo_root, ctx).await {
        ctx.log(format!("ci monitor reusing existing revert pr {}", url));
        return Some(url);
    }

    let worktree_root = repo_root.join(".worktrees");
    let _ = std::fs::create_dir_all(&worktree_root);
    let worktree_path = worktree_root.join(format!("ci-revert-{}", branch));
    remove_existing_revert_worktree(repo_root, &worktree_path, ctx).await;

    let (worktree_added, mut created_pr_url) = create_revert_pr_candidate(
        owner,
        repo,
        pr_number,
        base_branch,
        merge_sha,
        source_pr_url,
        &branch,
        repo_root,
        &worktree_path,
        ctx,
    )
    .await;

    if worktree_added {
        cleanup_ci_worktree(repo_root, &worktree_path, ctx).await;
    }

    if created_pr_url.is_none() {
        created_pr_url = lookup_revert_pr_url(owner, repo, &branch, "open", repo_root, ctx).await;
    }
    if created_pr_url.is_none() {
        created_pr_url = lookup_revert_pr_url(owner, repo, &branch, "all", repo_root, ctx).await;
    }
    created_pr_url
}

async fn remove_existing_revert_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    ctx: &mut CiMonitorContext,
) {
    if !worktree_path.exists() {
        return;
    }
    let worktree_path_s = worktree_path.to_string_lossy().to_string();
    let remove = run_command_capture(
        repo_root,
        "git",
        &args(&["worktree", "remove", "--force", &worktree_path_s]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_worktree_remove_revert", &remove);
}

#[allow(clippy::too_many_arguments)]
async fn create_revert_pr_candidate(
    owner: &str,
    repo: &str,
    pr_number: u64,
    base_branch: &str,
    merge_sha: &str,
    source_pr_url: &str,
    branch: &str,
    repo_root: &Path,
    worktree_path: &Path,
    ctx: &mut CiMonitorContext,
) -> (bool, Option<String>) {
    if !fetch_and_add_revert_worktree(repo_root, base_branch, worktree_path, ctx).await {
        return (false, None);
    }
    if !checkout_revert_branch(worktree_path, branch, ctx).await {
        return (true, None);
    }
    if !revert_and_push_merge_commit(worktree_path, merge_sha, branch, ctx).await {
        return (true, None);
    }

    let url = create_revert_pull_request(
        owner,
        repo,
        pr_number,
        base_branch,
        merge_sha,
        source_pr_url,
        branch,
        worktree_path,
        ctx,
    )
    .await;
    (true, url)
}

async fn fetch_and_add_revert_worktree(
    repo_root: &Path,
    base_branch: &str,
    worktree_path: &Path,
    ctx: &mut CiMonitorContext,
) -> bool {
    let fetch = run_command_capture(
        repo_root,
        "git",
        &args(&["fetch", "origin", base_branch]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_fetch_revert_base", &fetch);
    if !command_succeeded(&fetch) {
        return false;
    }

    let worktree_path_s = worktree_path.to_string_lossy().to_string();
    let add = run_command_capture(
        repo_root,
        "git",
        &args(&[
            "worktree",
            "add",
            &worktree_path_s,
            &format!("origin/{}", base_branch),
        ]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_worktree_add_revert", &add);
    command_succeeded(&add)
}

async fn checkout_revert_branch(
    worktree_path: &Path,
    branch: &str,
    ctx: &mut CiMonitorContext,
) -> bool {
    let checkout = run_command_capture(
        worktree_path,
        "git",
        &args(&["checkout", "-B", branch]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_checkout_revert_branch", &checkout);
    command_succeeded(&checkout)
}

async fn revert_and_push_merge_commit(
    worktree_path: &Path,
    merge_sha: &str,
    branch: &str,
    ctx: &mut CiMonitorContext,
) -> bool {
    let revert = run_command_capture(
        worktree_path,
        "git",
        &args(&["revert", "-m", "1", "--no-edit", merge_sha]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_revert_merge_commit", &revert);
    if !command_succeeded(&revert) {
        return false;
    }

    let push = run_command_capture(
        worktree_path,
        "git",
        &args(&["push", "origin", &format!("HEAD:{}", branch)]),
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record("git_push_revert_branch", &push);
    command_succeeded(&push)
}

#[allow(clippy::too_many_arguments)]
async fn create_revert_pull_request(
    owner: &str,
    repo: &str,
    pr_number: u64,
    base_branch: &str,
    merge_sha: &str,
    source_pr_url: &str,
    branch: &str,
    worktree_path: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<String> {
    let title = format!(
        "revert: merge {} after post-merge CI failure",
        &merge_sha[..merge_sha.len().min(12)]
    );
    let body = format!(
        "Automated revert for failed post-merge checks.\n\n- source_pr: {}\n- source_pr_number: {}\n- merge_commit: {}\n",
        source_pr_url, pr_number, merge_sha
    );
    let create = run_command_capture(
        worktree_path,
        "gh",
        &[
            "pr".to_string(),
            "create".to_string(),
            "--repo".to_string(),
            format!("{}/{}", owner, repo),
            "--base".to_string(),
            base_branch.to_string(),
            "--head".to_string(),
            branch.to_string(),
            "--title".to_string(),
            title,
            "--body".to_string(),
            body,
        ],
        240,
    )
    .await;
    ctx.record("gh_pr_create_revert", &create);
    if !command_succeeded(&create) {
        return None;
    }
    extract_pull_request_url(&create.stdout)
        .or_else(|| extract_pull_request_url(&command_combined_output(&create)))
}

async fn lookup_revert_pr_url(
    owner: &str,
    repo: &str,
    branch: &str,
    state: &str,
    repo_root: &Path,
    ctx: &mut CiMonitorContext,
) -> Option<String> {
    let run = run_command_capture(
        repo_root,
        "gh",
        &[
            "pr".to_string(),
            "list".to_string(),
            "--repo".to_string(),
            format!("{}/{}", owner, repo),
            "--state".to_string(),
            state.to_string(),
            "--head".to_string(),
            format!("{}:{}", owner, branch),
            "--json".to_string(),
            "url".to_string(),
        ],
        CI_COMMAND_TIMEOUT_SECS,
    )
    .await;
    ctx.record(&format!("gh_pr_list_revert_{}", state), &run);
    if !command_succeeded(&run) {
        return None;
    }
    parse_pr_list_first_url(&run.stdout)
}

fn parse_pr_url(pr_url: &str) -> Option<ParsedPrUrl> {
    let trimmed = pr_url.trim().trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() < 7 {
        return None;
    }
    if parts[0] != "https:" && parts[0] != "http:" {
        return None;
    }
    if parts[2] != "github.com" || parts[5] != "pull" {
        return None;
    }
    let owner = parts[3].trim();
    let repo = parts[4].trim();
    let number = parts[6].trim().parse::<u64>().ok()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(ParsedPrUrl {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

fn ci_auto_revert_branch(merge_sha: &str) -> String {
    format!(
        "ci-auto-revert-{}",
        merge_sha
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(12)
            .collect::<String>()
            .to_lowercase()
    )
}

fn parse_commit_check_runs(raw_json: &str) -> Option<Vec<CiCheckStatus>> {
    let response: CommitCheckRunsResponse = serde_json::from_str(raw_json.trim()).ok()?;
    let checks = response
        .check_runs
        .into_iter()
        .map(|run| CiCheckStatus {
            name: run.name.unwrap_or_else(|| "unknown".to_string()),
            status: run.status.unwrap_or_else(|| "unknown".to_string()),
            conclusion: run.conclusion.unwrap_or_default(),
            details_url: run.html_url,
        })
        .collect::<Vec<_>>();
    Some(checks)
}

fn compute_post_merge_overall(checks: &[CiCheckStatus]) -> String {
    if checks.is_empty() {
        return "pending".to_string();
    }
    compute_overall(checks)
}

fn parse_pr_list_first_url(raw_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw_json.trim()).ok()?;
    let arr = value.as_array()?;
    arr.iter()
        .find_map(|item| item.get("url").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn extract_pull_request_url(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"https?://github\.com/[^/\s]+/[^/\s]+/pull/\d+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

fn parse_status_check_rollup(raw_json: &str) -> Option<Vec<CiCheckStatus>> {
    let response: StatusCheckResponse = serde_json::from_str(raw_json.trim()).ok()?;
    let checks = response
        .status_check_rollup
        .into_iter()
        .map(|entry| CiCheckStatus {
            name: entry.name.unwrap_or_else(|| "unknown".to_string()),
            status: entry.status.unwrap_or_else(|| "unknown".to_string()),
            conclusion: entry.conclusion.unwrap_or_else(|| "".to_string()),
            details_url: entry.details_url,
        })
        .collect();
    Some(checks)
}

fn compute_overall(checks: &[CiCheckStatus]) -> String {
    if checks.is_empty() {
        return "passing".to_string();
    }

    let mut any_failure = false;
    let mut all_success = true;
    let mut any_pending = false;

    for check in checks {
        let status = check.status.trim().to_uppercase();
        let conclusion = check.conclusion.trim().to_uppercase();
        let is_failure = is_failure_conclusion(&conclusion);
        let is_success = is_success_conclusion(&conclusion);
        let is_pending = conclusion.is_empty()
            || matches!(status.as_str(), "IN_PROGRESS" | "QUEUED" | "PENDING");

        if is_failure {
            any_failure = true;
        }
        if !is_success {
            all_success = false;
        }
        if is_pending {
            any_pending = true;
        }
    }

    if any_failure {
        "failing".to_string()
    } else if all_success && !any_pending {
        "passing".to_string()
    } else {
        "pending".to_string()
    }
}

fn is_failure_conclusion(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE" | "ERROR"
    )
}

fn is_success_conclusion(conclusion: &str) -> bool {
    matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED")
}

fn parse_pr_checks(raw_json: &str) -> Vec<PrCheckEntry> {
    let value: serde_json::Value = match serde_json::from_str(raw_json.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if let Some(arr) = value.as_array() {
        return arr
            .iter()
            .filter_map(|v| serde_json::from_value::<PrCheckEntry>(v.clone()).ok())
            .collect();
    }
    if let Some(arr) = value.get("checks").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| serde_json::from_value::<PrCheckEntry>(v.clone()).ok())
            .collect();
    }
    Vec::new()
}

fn extract_run_id(link: &str) -> Option<String> {
    let re = regex::Regex::new(r"/runs/(\d+)").ok()?;
    if let Some(caps) = re.captures(link) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    let re = regex::Regex::new(r"/check-runs/(\d+)").ok()?;
    if let Some(caps) = re.captures(link) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    let re = regex::Regex::new(r"(\d{5,})").ok()?;
    re.find_iter(link)
        .last()
        .map(|m| m.as_str().to_string())
}

fn sanitize_branch(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn build_ci_monitor_command(name: &str, run: &CommandRunResult) -> CiMonitorCommand {
    let status = if run.timed_out {
        "timeout"
    } else if run.exit_code == Some(0) {
        "passed"
    } else {
        "failed"
    };
    CiMonitorCommand {
        name: name.to_string(),
        command: run.command.clone(),
        exit_code: run.exit_code,
        timed_out: run.timed_out,
        duration_ms: run.duration_ms,
        status: status.to_string(),
        stdout_tail: tail_text(&run.stdout, 1200),
        stderr_tail: tail_text(&run.stderr, 1200),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rollup_passing() {
        let raw = r#"{"statusCheckRollup":[{"name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://example.com"}]}"#;
        let checks = parse_status_check_rollup(raw).expect("parse");
        assert_eq!(checks.len(), 1);
        assert_eq!(compute_overall(&checks), "passing");
    }

    #[test]
    fn parse_rollup_failing() {
        let raw = r#"{"statusCheckRollup":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}"#;
        let checks = parse_status_check_rollup(raw).expect("parse");
        assert_eq!(compute_overall(&checks), "failing");
    }

    #[test]
    fn parse_rollup_pending() {
        let raw = r#"{"statusCheckRollup":[{"name":"lint","status":"IN_PROGRESS","conclusion":null}]}"#;
        let checks = parse_status_check_rollup(raw).expect("parse");
        assert_eq!(compute_overall(&checks), "pending");
    }

    #[test]
    fn parse_rollup_empty_is_passing() {
        let raw = r#"{"statusCheckRollup":[]}"#;
        let checks = parse_status_check_rollup(raw).expect("parse");
        assert_eq!(compute_overall(&checks), "passing");
    }

    #[test]
    fn parse_rollup_skipped_is_passing() {
        let raw = r#"{"statusCheckRollup":[{"name":"docs","status":"COMPLETED","conclusion":"SKIPPED"}]}"#;
        let checks = parse_status_check_rollup(raw).expect("parse");
        assert_eq!(compute_overall(&checks), "passing");
    }

    #[test]
    fn parse_pr_url_extracts_owner_repo_and_number() {
        let parsed = parse_pr_url("https://github.com/Enreign/athena/pull/48")
            .expect("parse pr url");
        assert_eq!(parsed.owner, "Enreign");
        assert_eq!(parsed.repo, "athena");
        assert_eq!(parsed.number, 48);
    }

    #[test]
    fn parse_pr_url_rejects_invalid_url() {
        assert!(parse_pr_url("https://example.com/Enreign/athena/pull/48").is_none());
        assert!(parse_pr_url("https://github.com/Enreign/athena/issues/48").is_none());
    }

    #[test]
    fn parse_commit_check_runs_maps_payload() {
        let raw = r#"{"check_runs":[{"name":"ci","status":"completed","conclusion":"success","html_url":"https://x"}]}"#;
        let checks = parse_commit_check_runs(raw).expect("parse check runs");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "ci");
        assert_eq!(checks[0].status, "completed");
        assert_eq!(checks[0].conclusion, "success");
        assert_eq!(checks[0].details_url.as_deref(), Some("https://x"));
    }

    #[test]
    fn parse_commit_check_runs_invalid_json_returns_none() {
        assert!(parse_commit_check_runs("{not_json").is_none());
    }

    #[test]
    fn compute_post_merge_overall_treats_empty_as_pending() {
        assert_eq!(compute_post_merge_overall(&[]), "pending");
    }

    #[test]
    fn parse_pr_list_first_url_returns_first_entry() {
        let raw = r#"[{"url":"https://github.com/Enreign/athena/pull/100"},{"url":"https://github.com/Enreign/athena/pull/101"}]"#;
        assert_eq!(
            parse_pr_list_first_url(raw).as_deref(),
            Some("https://github.com/Enreign/athena/pull/100")
        );
    }

    #[test]
    fn ci_auto_revert_branch_is_stable_for_same_sha() {
        let sha = "ABCDEF1234567890ABCDEF1234567890ABCDEF12";
        assert_eq!(
            ci_auto_revert_branch(sha),
            "ci-auto-revert-abcdef123456".to_string()
        );
    }

    #[test]
    fn extract_pull_request_url_finds_url_in_output() {
        let out = "Created pull request:\nhttps://github.com/Enreign/athena/pull/55\n";
        assert_eq!(
            extract_pull_request_url(out).as_deref(),
            Some("https://github.com/Enreign/athena/pull/55")
        );
    }
}
