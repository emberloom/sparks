#!/usr/bin/env python3
"""
Eval harness for Athena mission lanes.

Runs a fixed benchmark suite of tasks via `athena dispatch`, scores each run,
and emits machine-readable and human-readable reports.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


TERMINAL_STATUSES = {"succeeded", "failed", "rolled_back"}
OUTCOME_REASON_WAIT_TIMEOUT = "outcome_wait_timeout"


def _to_text(value: object) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


@dataclass
class TaskResult:
    task_id: str
    lane: str
    risk: str
    ghost: str
    cli_tool: str | None
    cli_model: str | None
    dispatch_task_id: str | None
    status: str
    error: str | None
    exec_success: float
    plan_quality: float
    tests_pass: float
    diff_quality: float
    overall: float
    changed_files: list[str]
    stdout: str
    stderr: str
    notes: list[str]


def run(
    cmd: list[str],
    cwd: Path,
    env: dict[str, str] | None = None,
    timeout_secs: int | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            cmd,
            cwd=str(cwd),
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout_secs,
        )
    except subprocess.TimeoutExpired as e:
        return subprocess.CompletedProcess(
            args=cmd,
            returncode=124,
            stdout=_to_text(e.stdout),
            stderr=_to_text(e.stderr) + f"\nTimeout after {timeout_secs}s",
        )


def run_shell(
    cmd: str,
    cwd: Path,
    env: dict[str, str] | None = None,
    timeout_secs: int | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            cmd,
            cwd=str(cwd),
            env=env,
            text=True,
            capture_output=True,
            check=False,
            shell=True,
            timeout=timeout_secs,
        )
    except subprocess.TimeoutExpired as e:
        return subprocess.CompletedProcess(
            args=cmd,
            returncode=124,
            stdout=_to_text(e.stdout),
            stderr=_to_text(e.stderr) + f"\nTimeout after {timeout_secs}s",
        )


def parse_db_path(config_path: Path) -> Path:
    default = Path("~/.athena/athena.db").expanduser()
    if not config_path.exists():
        return default
    text = config_path.read_text()
    if tomllib is not None:
        data = tomllib.loads(text)
        db = data.get("db", {})
        raw = db.get("path")
        if raw:
            return Path(raw).expanduser()

    # Fallback parser for Python < 3.11 (no tomllib).
    in_db = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            in_db = line == "[db]"
            continue
        if in_db and line.startswith("path"):
            _, rhs = line.split("=", 1)
            value = rhs.strip().strip('"').strip("'")
            if value:
                return Path(value).expanduser()
    return default


def lane_timeout_budget(lane: str, risk: str, fast_mode: bool = False) -> tuple[int, int, int]:
    if fast_mode:
        return 120, 120, 240
    lane_key = lane.strip().lower()
    risk_key = risk.strip().lower()
    if lane_key == "delivery":
        wait_map = {"low": 420, "medium": 600, "high": 900}
        wait = wait_map.get(risk_key, 600)
        return wait, max(wait, 480), max(wait + 240, 900)
    if lane_key == "self_improvement":
        wait_map = {"low": 240, "medium": 360, "high": 540}
        wait = wait_map.get(risk_key, 360)
        return wait, max(wait, 300), max(wait + 180, 600)
    wait = 300
    return wait, 360, 600


def git_status_paths(repo: Path) -> set[str]:
    p = run(["git", "status", "--porcelain"], cwd=repo)
    if p.returncode != 0:
        return set()
    out: set[str] = set()
    for line in p.stdout.splitlines():
        if not line:
            continue
        path = line[3:]
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        out.add(path.strip())
    return out


def parse_dispatch_task_id(stderr: str) -> str | None:
    m = re.search(
        r"task_id=([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})",
        stderr,
    )
    return m.group(1) if m else None


def query_outcome(conn: sqlite3.Connection, dispatch_task_id: str | None) -> dict[str, Any]:
    if not dispatch_task_id:
        return {}
    row = conn.execute(
        """
        SELECT status, error, verification_total, verification_passed, rolled_back
        FROM autonomous_task_outcomes
        WHERE task_id = ?
        """,
        (dispatch_task_id,),
    ).fetchone()
    if row is None:
        return {}
    return {
        "status": row[0],
        "error": row[1],
        "verification_total": row[2],
        "verification_passed": row[3],
        "rolled_back": row[4],
    }


def wait_for_terminal_outcome(
    conn: sqlite3.Connection,
    dispatch_task_id: str,
    max_wait_secs: int,
    poll_secs: float = 2.0,
) -> tuple[dict[str, Any], bool]:
    deadline = time.monotonic() + max_wait_secs
    latest: dict[str, Any] = {}
    while time.monotonic() < deadline:
        latest = query_outcome(conn, dispatch_task_id)
        if latest.get("status") in TERMINAL_STATUSES:
            return latest, True
        time.sleep(poll_secs)
    latest = query_outcome(conn, dispatch_task_id)
    return latest, latest.get("status") in TERMINAL_STATUSES


def fail_outcome_if_started(conn: sqlite3.Connection, dispatch_task_id: str, error: str) -> bool:
    cur = conn.execute(
        """
        UPDATE autonomous_task_outcomes
           SET status = 'failed',
               finished_at = datetime('now'),
               error = COALESCE(error, ?2)
         WHERE task_id = ?1
           AND status = 'started'
        """,
        (dispatch_task_id, error),
    )
    conn.commit()
    return cur.rowcount > 0


def score_plan_quality(response: str) -> float:
    text = response.strip()
    if not text:
        return 0.0
    structured = score_plan_quality_structured(text)
    if structured is not None:
        return structured
    return score_plan_quality_legacy(text)


def score_plan_quality_structured(text: str) -> float | None:
    plan = _parse_structured_plan(text)
    if plan is None:
        return None

    score = 0.0
    if _has_non_empty(plan, ("plan", "steps")):
        score += 0.4
    if _has_non_empty(plan, ("execution", "execute", "actions")):
        score += 0.3
    if _count_structured_steps(plan) >= 2:
        score += 0.2
    if _has_non_empty(
        plan,
        ("verification", "verify", "tests", "test_plan", "rollback", "rollback_plan"),
    ):
        score += 0.1
    return min(score, 1.0)


def score_plan_quality_legacy(text: str) -> float:
    score = 0.0
    if "PLAN:" in text:
        score += 0.4
    if "EXECUTION:" in text:
        score += 0.3
    numbered_steps = len(re.findall(r"(?m)^\s*(?:\d+[.)]|[-*])\s+", text))
    if numbered_steps >= 2:
        score += 0.2
    if any(k in text.lower() for k in ("verify", "test", "check", "rollback")):
        score += 0.1
    return min(score, 1.0)


def _parse_structured_plan(text: str) -> dict[str, Any] | None:
    candidates: list[str] = []
    stripped = text.strip()
    if stripped.startswith("{") and stripped.endswith("}"):
        candidates.append(stripped)

    for match in re.finditer(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL):
        candidates.append(match.group(1).strip())

    for candidate in candidates:
        try:
            parsed = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict) and _has_non_empty(
            parsed,
            (
                "plan",
                "steps",
                "execution",
                "execute",
                "actions",
                "verification",
                "verify",
                "tests",
                "test_plan",
                "rollback",
                "rollback_plan",
            ),
        ):
            return parsed
    return None


def _has_non_empty(plan: dict[str, Any], keys: tuple[str, ...]) -> bool:
    for key in keys:
        value = plan.get(key)
        if isinstance(value, list) and value:
            return True
        if isinstance(value, str) and value.strip():
            return True
        if isinstance(value, dict) and value:
            return True
    return False


def _count_structured_steps(plan: dict[str, Any]) -> int:
    for key in ("steps", "plan"):
        value = plan.get(key)
        if isinstance(value, list):
            return len(value)
        if isinstance(value, str) and value.strip():
            return len(re.findall(r"(?m)^\s*(?:\d+[.)]|[-*])\s+", value)) or len(
                [line for line in value.splitlines() if line.strip()]
            )
    return 0


def score_diff_quality(
    repo: Path,
    task: dict[str, Any],
    changed_files: list[str],
    response_text: str,
) -> tuple[float, list[str]]:
    expect = task.get("expect", {})
    notes: list[str] = []
    score = 1.0

    if expect.get("no_file_changes"):
        if changed_files:
            return 0.0, [f"Expected no file changes, found: {changed_files}"]
        return 1.0, []

    max_changed = expect.get("max_changed_files")
    if isinstance(max_changed, int) and len(changed_files) > max_changed:
        score -= 0.35
        notes.append(f"Changed files {len(changed_files)} exceeds max {max_changed}.")

    allow = expect.get("changed_files_allow", [])
    if allow:
        illegal = [p for p in changed_files if p not in allow]
        if illegal:
            score -= 0.35
            notes.append(f"Unexpected changed files: {illegal}")

    diff_check = run(["git", "diff", "--check"], cwd=repo)
    if diff_check.returncode != 0:
        score -= 0.3
        notes.append("git diff --check reported whitespace/merge issues.")

    diff_text = run(["git", "diff", "--", *changed_files], cwd=repo).stdout if changed_files else ""
    if re.search(r"\b(?:TODO|FIXME)\b", diff_text):
        score -= 0.2
        notes.append("Diff introduced TODO/FIXME markers.")

    must_contain = expect.get("must_contain", [])
    for mc in must_contain:
        path = repo / mc.get("file", "")
        text = mc.get("text", "")
        if not path.exists() or text not in path.read_text(errors="ignore"):
            score -= 0.25
            notes.append(f"Expected text missing: {mc}")

    response_patterns = expect.get("response_patterns", [])
    for pattern in response_patterns:
        if not re.search(pattern, response_text):
            score -= 0.2
            notes.append(f"Response pattern not matched: {pattern}")

    return max(0.0, score), notes


def score_tests(repo: Path, test_command: str) -> tuple[float, str]:
    if not test_command:
        return 1.0, "skipped"
    p = run_shell(test_command, cwd=repo, timeout_secs=900)
    if p.returncode == 0:
        return 1.0, "passed"
    return 0.0, (p.stderr or p.stdout or "failed").strip()[:500]


def _min_rule_value(rule: dict[str, Any], key: str, default: float) -> float:
    raw = rule.get(key, default)
    try:
        return float(raw)
    except (TypeError, ValueError):
        return default


def evaluate_gate(
    results: list[TaskResult],
    threshold: float,
    suite: dict[str, Any],
    overall: float,
) -> tuple[bool, list[str]]:
    reasons: list[str] = []
    gate_cfg = suite.get("gate_requirements", {}) or {}
    min_overall = _min_rule_value(gate_cfg, "min_overall", threshold)
    require_exec_success = bool(gate_cfg.get("require_exec_success", True))
    lane_rules = gate_cfg.get("lane_rules", {}) or {}
    task_rules = gate_cfg.get("task_rules", {}) or {}

    if overall < min_overall:
        reasons.append(f"overall_below_threshold overall={overall:.2f} min_overall={min_overall:.2f}")

    for r in results:
        if require_exec_success and r.exec_success < 1.0:
            reasons.append(f"task={r.task_id} exec_success<{1.0:.2f} actual={r.exec_success:.2f}")

        effective_rules: list[tuple[str, dict[str, Any]]] = []
        lane_rule = lane_rules.get(r.lane)
        if isinstance(lane_rule, dict):
            effective_rules.append((f"lane:{r.lane}", lane_rule))
        task_rule = task_rules.get(r.task_id)
        if isinstance(task_rule, dict):
            effective_rules.append((f"task:{r.task_id}", task_rule))

        for scope, rule in effective_rules:
            min_tests = _min_rule_value(rule, "min_tests_pass", 0.0)
            min_diff = _min_rule_value(rule, "min_diff_quality", 0.0)
            min_plan = _min_rule_value(rule, "min_plan_quality", 0.0)
            min_overall_task = _min_rule_value(rule, "min_task_overall", 0.0)
            if r.tests_pass < min_tests:
                reasons.append(
                    f"{scope} task={r.task_id} tests_pass<{min_tests:.2f} actual={r.tests_pass:.2f}"
                )
            if r.diff_quality < min_diff:
                reasons.append(
                    f"{scope} task={r.task_id} diff_quality<{min_diff:.2f} actual={r.diff_quality:.2f}"
                )
            if r.plan_quality < min_plan:
                reasons.append(
                    f"{scope} task={r.task_id} plan_quality<{min_plan:.2f} actual={r.plan_quality:.2f}"
                )
            if r.overall < min_overall_task:
                reasons.append(
                    f"{scope} task={r.task_id} overall<{min_overall_task:.2f} actual={r.overall:.2f}"
                )

    return len(reasons) == 0, reasons


def build_dispatch_goal(raw_goal: str) -> str:
    return (
        "Final response format:\n"
        "PLAN:\n"
        "- step 1\n"
        "- step 2\n"
        "EXECUTION:\n"
        "- what was done\n"
        "- verification\n\n"
        f"Task:\n{raw_goal}"
    )


def safe_task_key(task_id: str) -> str:
    return re.sub(r"[^a-zA-Z0-9_.-]+", "_", task_id).strip("_") or "task"


def create_task_workspace(base_repo: Path, task_id: str, ref: str) -> Path:
    worktree_root = base_repo / "eval" / ".worktrees"
    worktree_root.mkdir(parents=True, exist_ok=True)
    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    path = worktree_root / f"{ts}-{safe_task_key(task_id)}"
    p = run(["git", "worktree", "add", "--detach", str(path), ref], cwd=base_repo)
    if p.returncode != 0:
        raise RuntimeError(f"git worktree add failed: {p.stderr or p.stdout}")
    return path


def remove_task_workspace(base_repo: Path, path: Path) -> None:
    run(["git", "worktree", "remove", "--force", str(path)], cwd=base_repo)


def managed_worktree_paths(base_repo: Path) -> list[Path]:
    root = (base_repo / "eval" / ".worktrees").resolve()
    p = run(["git", "worktree", "list", "--porcelain"], cwd=base_repo)
    if p.returncode != 0:
        return []
    paths: list[Path] = []
    for line in p.stdout.splitlines():
        if not line.startswith("worktree "):
            continue
        candidate = Path(line.split(" ", 1)[1]).resolve()
        try:
            candidate.relative_to(root)
        except ValueError:
            continue
        paths.append(candidate)
    return paths


def cleanup_stale_worktrees(base_repo: Path, stale_hours: float) -> tuple[int, int]:
    if stale_hours <= 0:
        return 0, 0
    stale_secs = stale_hours * 3600.0
    now = time.time()

    run(["git", "worktree", "prune"], cwd=base_repo)
    removed = 0
    failed = 0
    for wt in managed_worktree_paths(base_repo):
        if not wt.exists():
            continue
        age_secs = now - wt.stat().st_mtime
        if age_secs < stale_secs:
            continue
        p = run(["git", "worktree", "remove", "--force", str(wt)], cwd=base_repo)
        if p.returncode == 0:
            removed += 1
        else:
            failed += 1
    run(["git", "worktree", "prune"], cwd=base_repo)
    return removed, failed


def run_task(
    repo: Path,
    athena_bin: Path,
    config_path: Path,
    conn: sqlite3.Connection,
    defaults: dict[str, Any],
    task: dict[str, Any],
    cli_tool: str | None,
    cli_model: str | None,
    dispatch_context: str | None,
    cli_timeout_secs: int,
) -> TaskResult:
    merged = dict(defaults)
    merged.update(task)

    ghost = str(merged.get("ghost", "coder"))
    lane = str(merged.get("lane", "delivery"))
    risk = str(merged.get("risk", "medium"))
    repo_name = str(merged.get("repo", "athena"))
    strict_timeout_budget = bool(merged.get("strict_timeout_budget", True))
    fast_mode = bool(dispatch_context and "[benchmark_fast_cli]" in dispatch_context.lower())
    min_wait_secs, min_outcome_wait_secs, min_timeout_secs = lane_timeout_budget(
        lane, risk, fast_mode=fast_mode
    )
    wait_raw = int(merged.get("wait_secs", min_wait_secs))
    outcome_wait_raw = int(merged.get("outcome_wait_secs", max(wait_raw, 120)))
    timeout_raw = int(merged.get("timeout_secs", wait_raw + 180))
    if strict_timeout_budget:
        wait_secs = max(wait_raw, min_wait_secs)
        outcome_wait_secs = max(outcome_wait_raw, min_outcome_wait_secs)
        timeout_secs = max(timeout_raw, min_timeout_secs, wait_secs + 60)
    else:
        wait_secs = max(wait_raw, 1)
        outcome_wait_secs = max(outcome_wait_raw, 1)
        timeout_secs = max(timeout_raw, 1)
    goal = build_dispatch_goal(str(merged.get("goal", "")).strip())
    test_command = str(merged.get("test_command", "")).strip()
    if lane == "delivery" and not test_command:
        test_command = "cargo check -q"
    task_name = str(merged.get("id", "unknown"))

    before = git_status_paths(repo)
    env = os.environ.copy()
    if cli_timeout_secs > 0:
        env["ATHENA_CLI_TIMEOUT_SECS"] = str(cli_timeout_secs)

    cmd = [
        str(athena_bin),
        "--config",
        str(config_path),
        "dispatch",
        "--ghost",
        ghost,
        "--goal",
        goal,
        "--wait-secs",
        str(wait_secs),
        "--lane",
        lane,
        "--risk",
        risk,
        "--repo",
        repo_name,
    ]
    if dispatch_context:
        cmd.extend(["--context", dispatch_context])
    if cli_tool:
        cmd.extend(["--cli-tool", cli_tool])
    if cli_model:
        cmd.extend(["--cli-model", cli_model])
    p = run(cmd, cwd=repo, env=env, timeout_secs=timeout_secs)
    dispatch_task_id = parse_dispatch_task_id(p.stderr)
    outcome = query_outcome(conn, dispatch_task_id)

    outcome_terminal = outcome.get("status") in TERMINAL_STATUSES
    if dispatch_task_id and not outcome_terminal:
        outcome, outcome_terminal = wait_for_terminal_outcome(
            conn, dispatch_task_id, max_wait_secs=outcome_wait_secs
        )
        if not outcome_terminal:
            if fail_outcome_if_started(conn, dispatch_task_id, OUTCOME_REASON_WAIT_TIMEOUT):
                outcome = query_outcome(conn, dispatch_task_id)
                outcome_terminal = outcome.get("status") in TERMINAL_STATUSES

    status = outcome.get("status", "unknown")
    error = outcome.get("error")

    after = git_status_paths(repo)
    changed = sorted(after - before)

    response_text = p.stdout.strip()
    exec_success = 1.0 if status == "succeeded" else 0.0
    plan_quality = score_plan_quality(response_text)
    tests_pass, test_note = score_tests(repo, test_command)
    diff_quality, diff_notes = score_diff_quality(repo, merged, changed, response_text)

    weights = {"exec_success": 0.35, "tests_pass": 0.25, "diff_quality": 0.25, "plan_quality": 0.15}
    overall = (
        exec_success * weights["exec_success"]
        + tests_pass * weights["tests_pass"]
        + diff_quality * weights["diff_quality"]
        + plan_quality * weights["plan_quality"]
    )

    notes = [f"test_command={test_note}"]
    notes.append(
        f"timeout_budget(wait={wait_secs}s,outcome_wait={outcome_wait_secs}s,subprocess={timeout_secs}s)"
    )
    if cli_tool:
        notes.append(f"cli_tool={cli_tool}")
    if cli_model:
        notes.append(f"cli_model={cli_model}")
    if dispatch_context:
        notes.append(f"dispatch_context={dispatch_context}")
    if cli_timeout_secs > 0:
        notes.append(f"cli_timeout_secs={cli_timeout_secs}")
    notes.extend(diff_notes)
    if p.returncode != 0:
        notes.append(f"dispatch_exit={p.returncode}")
    if dispatch_task_id and not outcome_terminal:
        notes.append(f"outcome_not_terminal_after={outcome_wait_secs}s")

    return TaskResult(
        task_id=task_name,
        lane=lane,
        risk=risk,
        ghost=ghost,
        cli_tool=cli_tool,
        cli_model=cli_model,
        dispatch_task_id=dispatch_task_id,
        status=status,
        error=error,
        exec_success=exec_success,
        plan_quality=plan_quality,
        tests_pass=tests_pass,
        diff_quality=diff_quality,
        overall=overall,
        changed_files=changed,
        stdout=p.stdout,
        stderr=p.stderr,
        notes=notes,
    )


def write_reports(
    output_dir: Path,
    suite: dict[str, Any],
    results: list[TaskResult],
    gate_ok: bool,
    threshold: float,
    gate_reasons: list[str],
    cli_tool: str | None,
    cli_model: str | None,
    dispatch_context: str | None,
    cli_timeout_secs: int,
) -> tuple[Path, Path, float, str]:
    output_dir.mkdir(parents=True, exist_ok=True)
    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    out_json = output_dir / f"eval-{ts}.json"
    out_md = output_dir / f"eval-{ts}.md"

    overall = sum(r.overall for r in results) / max(len(results), 1)
    payload = {
        "timestamp_utc": ts,
        "suite": suite.get("name"),
        "threshold": threshold,
        "gate_ok": gate_ok,
        "gate_reasons": gate_reasons,
        "overall_score": overall,
        "cli_tool": cli_tool,
        "cli_model": cli_model,
        "dispatch_context": dispatch_context,
        "cli_timeout_secs": cli_timeout_secs,
        "results": [r.__dict__ for r in results],
    }
    out_json.write_text(json.dumps(payload, indent=2))

    lines = [
        f"# Eval Report: {suite.get('name')}",
        "",
        f"- timestamp_utc: {ts}",
        f"- threshold: {threshold:.2f}",
        f"- overall_score: {overall:.2f}",
        f"- gate: {'PASS' if gate_ok else 'FAIL'}",
        f"- cli_tool: {cli_tool or 'default'}",
        f"- cli_model: {cli_model or 'default'}",
        f"- dispatch_context: {dispatch_context or 'default'}",
        f"- cli_timeout_secs: {cli_timeout_secs if cli_timeout_secs > 0 else 'default'}",
        "",
        "| task | lane | risk | status | overall | exec | tests | diff | plan |",
        "|---|---|---|---|---:|---:|---:|---:|---:|",
    ]
    for r in results:
        lines.append(
            f"| `{r.task_id}` | `{r.lane}` | `{r.risk}` | `{r.status}` | "
            f"{r.overall:.2f} | {r.exec_success:.2f} | {r.tests_pass:.2f} | {r.diff_quality:.2f} | {r.plan_quality:.2f} |"
        )
    lines.append("")
    lines.append("## Gate Reasons")
    if gate_reasons:
        for reason in gate_reasons:
            lines.append(f"- {reason}")
    else:
        lines.append("- none")
    lines.append("")
    lines.append("## Notes")
    for r in results:
        lines.append(f"- `{r.task_id}`: {'; '.join(r.notes) if r.notes else 'none'}")
    out_md.write_text("\n".join(lines) + "\n")

    print(f"report_json={out_json}")
    print(f"report_md={out_md}")
    return out_json, out_md, overall, ts


def append_history(
    history_path: Path,
    suite_name: str,
    threshold: float,
    gate_ok: bool,
    gate_reasons: list[str],
    overall: float,
    timestamp_utc: str,
    report_json: Path,
    report_md: Path,
    results: list[TaskResult],
    cli_tool: str | None,
    cli_model: str | None,
    dispatch_context: str | None,
    cli_timeout_secs: int,
) -> None:
    history_path.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "timestamp_utc": timestamp_utc,
        "suite": suite_name,
        "threshold": threshold,
        "gate_ok": gate_ok,
        "gate_reasons": gate_reasons,
        "overall_score": overall,
        "task_count": len(results),
        "exec_success_rate": (
            sum(r.exec_success for r in results) / max(len(results), 1)
        ),
        "cli_tool": cli_tool,
        "cli_model": cli_model,
        "dispatch_context": dispatch_context,
        "cli_timeout_secs": cli_timeout_secs,
        "report_json": str(report_json),
        "report_md": str(report_md),
        "tasks": [
            {
                "task_id": r.task_id,
                "lane": r.lane,
                "risk": r.risk,
                "status": r.status,
                "overall": r.overall,
                "dispatch_task_id": r.dispatch_task_id,
            }
            for r in results
        ],
    }
    with history_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=True) + "\n")
    print(f"history_jsonl={history_path}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Athena fixed benchmark eval harness.")
    parser.add_argument("--suite", default="eval/benchmark-suite.json")
    parser.add_argument("--config", default="config.toml")
    parser.add_argument("--athena-bin", default="target/debug/athena")
    parser.add_argument("--output-dir", default="eval/results")
    parser.add_argument("--history-file", default="eval/results/history.jsonl")
    parser.add_argument(
        "--cli-tool",
        choices=["claude_code", "codex", "opencode"],
        default=None,
        help="Override Athena runtime cli_tool for each dispatch in this harness run.",
    )
    parser.add_argument(
        "--cli-model",
        default=None,
        help="Optional model name to pass as runtime cli_model override for each dispatch.",
    )
    parser.add_argument(
        "--dispatch-context",
        default=None,
        help="Optional dispatch context string applied to each task run.",
    )
    parser.add_argument(
        "--cli-timeout-secs",
        type=int,
        default=0,
        help="If >0, set ATHENA_CLI_TIMEOUT_SECS for each task run.",
    )
    parser.add_argument("--fail-fast", action="store_true")
    parser.add_argument("--max-tasks", type=int, default=0, help="Run only first N tasks (0 = all).")
    parser.add_argument("--worktree-ref", default="HEAD")
    parser.add_argument("--keep-worktrees", action="store_true")
    parser.add_argument(
        "--cleanup-worktrees",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Remove stale disposable eval worktrees before running tasks.",
    )
    parser.add_argument(
        "--stale-worktree-hours",
        type=float,
        default=6.0,
        help="Age threshold for stale eval worktree cleanup.",
    )
    parser.add_argument(
        "--use-worktree",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Run each benchmark task in an isolated disposable git worktree.",
    )
    return parser.parse_args()


def resolve_path(repo: Path, p: str) -> Path:
    raw = Path(p)
    if raw.is_absolute():
        return raw
    return (repo / raw).resolve()


def main() -> int:
    args = parse_args()
    repo = Path.cwd().resolve()
    suite_path = resolve_path(repo, args.suite)
    config_path = resolve_path(repo, args.config)
    athena_bin = resolve_path(repo, args.athena_bin)
    output_dir = resolve_path(repo, args.output_dir)
    history_path = resolve_path(repo, args.history_file)

    if not suite_path.exists():
        print(f"Suite not found: {suite_path}", file=sys.stderr)
        return 2
    if not config_path.exists():
        print(f"Config not found: {config_path}", file=sys.stderr)
        return 2
    if not athena_bin.exists():
        print(f"Athena binary not found: {athena_bin}", file=sys.stderr)
        return 2

    suite = json.loads(suite_path.read_text())
    defaults = suite.get("defaults", {})
    threshold = float(suite.get("pass_threshold", 0.7))
    db_path = parse_db_path(config_path)
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    tasks = list(suite.get("tasks", []))
    if args.max_tasks and args.max_tasks > 0:
        tasks = tasks[: args.max_tasks]
    print(
        f"run_config suite={suite.get('name')} tasks={len(tasks)} "
        f"cli_tool={args.cli_tool or 'default'} cli_model={args.cli_model or 'default'} "
        f"dispatch_context={args.dispatch_context or 'default'} cli_timeout_secs={args.cli_timeout_secs}",
        flush=True,
    )

    if args.use_worktree and args.cleanup_worktrees:
        removed, failed = cleanup_stale_worktrees(repo, args.stale_worktree_hours)
        print(
            f"worktree_cleanup removed={removed} failed={failed} stale_hours={args.stale_worktree_hours:g}",
            flush=True,
        )

    results: list[TaskResult] = []
    for task in tasks:
        task_id = str(task.get("id", "unknown"))
        print(f"running_task={task_id}", flush=True)

        workspace = repo
        if args.use_worktree:
            workspace = create_task_workspace(repo, task_id, args.worktree_ref)

        try:
            result = run_task(
                workspace,
                athena_bin,
                config_path,
                conn,
                defaults,
                task,
                args.cli_tool,
                args.cli_model,
                args.dispatch_context,
                args.cli_timeout_secs,
            )
        finally:
            if args.use_worktree and not args.keep_worktrees:
                remove_task_workspace(repo, workspace)

        results.append(result)
        print(
            f"task={task_id} status={result.status} overall={result.overall:.2f} "
            f"exec={result.exec_success:.2f} tests={result.tests_pass:.2f} "
            f"diff={result.diff_quality:.2f} plan={result.plan_quality:.2f}",
            flush=True,
        )

        if args.fail_fast and result.overall < threshold:
            break

    conn.close()

    overall = sum(r.overall for r in results) / max(len(results), 1)
    gate_ok, gate_reasons = evaluate_gate(results, threshold, suite, overall)
    report_json, report_md, overall, ts = write_reports(
        output_dir,
        suite,
        results,
        gate_ok,
        threshold,
        gate_reasons,
        args.cli_tool,
        args.cli_model,
        args.dispatch_context,
        args.cli_timeout_secs,
    )
    append_history(
        history_path=history_path,
        suite_name=str(suite.get("name", "unknown")),
        threshold=threshold,
        gate_ok=gate_ok,
        gate_reasons=gate_reasons,
        overall=overall,
        timestamp_utc=ts,
        report_json=report_json,
        report_md=report_md,
        results=results,
        cli_tool=args.cli_tool,
        cli_model=args.cli_model,
        dispatch_context=args.dispatch_context,
        cli_timeout_secs=args.cli_timeout_secs,
    )
    print(f"gate={'PASS' if gate_ok else 'FAIL'} overall={overall:.2f}")
    return 0 if gate_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
