#!/usr/bin/env python3
"""
Run a Phase 4 optimizer tournament across prompt/policy candidates.

This script evaluates candidate dispatch-context variants on a fixed benchmark
suite using `scripts/eval_harness.py`, then selects a winner with explicit
regression gates and provenance artifacts.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Any


@dataclass
class CandidateSpec:
    candidate_id: str
    source: str
    dispatch_context: str
    hypothesis: str


@dataclass
class CandidateResult:
    candidate_id: str
    source: str
    hypothesis: str
    dispatch_context: str
    command: list[str]
    exit_code: int
    report_json: str | None
    gate_ok: bool
    overall_score: float
    exec_success_rate: float
    avg_task_overall: float
    task_count: int
    error: str | None


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Run Athena optimizer tournament.")
    p.add_argument("--suite", default="eval/benchmark-real-gate.json")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--athena-bin", default="target/debug/athena")
    p.add_argument("--output-dir", default="eval/results")
    p.add_argument("--history-file", default="eval/results/history.jsonl")
    p.add_argument("--cli-tool", default="codex")
    p.add_argument("--cli-model", default=None)
    p.add_argument("--dispatch-context-base", default="[optimizer_tournament]")
    p.add_argument("--cli-timeout-secs", type=int, default=180)
    p.add_argument("--max-tasks", type=int, default=0)
    p.add_argument("--worktree-ref", default="HEAD")
    p.add_argument("--keep-worktrees", action="store_true")
    p.add_argument("--no-use-worktree", action="store_true")
    p.add_argument("--no-cleanup-worktrees", action="store_true")
    p.add_argument("--backlog-json", default="eval/results/improvement-backlog-latest.json")
    p.add_argument("--regen-backlog", action="store_true")
    p.add_argument("--top-backlog", type=int, default=2)
    p.add_argument("--max-candidates", type=int, default=5)
    p.add_argument("--min-improvement", type=float, default=0.01)
    p.add_argument("--max-regression", type=float, default=0.02)
    p.add_argument("--dry-run", action="store_true")
    return p.parse_args()


def parse_report_json_path(output: str) -> str | None:
    m = re.search(r"report_json=(.+)", output)
    return m.group(1).strip() if m else None


def load_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except Exception:
        return {}


def run_cmd(cmd: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        text=True,
        capture_output=True,
        check=False,
    )


def maybe_regen_backlog(repo: Path, args: argparse.Namespace) -> None:
    if not args.regen_backlog:
        return
    cmd = [sys.executable, "scripts/generate_improvement_backlog.py", "--out-dir", args.output_dir]
    p = run_cmd(cmd, repo)
    if p.returncode != 0:
        sys.stderr.write(p.stdout or "")
        sys.stderr.write(p.stderr or "")
        raise SystemExit(p.returncode or 1)


def default_candidates(base_context: str) -> list[CandidateSpec]:
    return [
        CandidateSpec(
            candidate_id="baseline",
            source="baseline",
            dispatch_context=base_context,
            hypothesis="Control candidate with baseline dispatch context.",
        ),
        CandidateSpec(
            candidate_id="mutation_plan_verify",
            source="mutation",
            dispatch_context=(
                f"{base_context}\n"
                "Execution contract: return PLAN and EXECUTION sections. "
                "Run required verification commands before final completion."
            ),
            hypothesis="Improve structure and verification compliance.",
        ),
        CandidateSpec(
            candidate_id="mutation_minimal_diff",
            source="mutation",
            dispatch_context=(
                f"{base_context}\n"
                "Change policy: prefer smallest safe diff, preserve existing architecture, "
                "and avoid unrelated edits."
            ),
            hypothesis="Reduce noisy diffs and improve diff-quality scores.",
        ),
        CandidateSpec(
            candidate_id="mutation_blocker_evidence",
            source="mutation",
            dispatch_context=(
                f"{base_context}\n"
                "If blocked, report deterministic evidence: command, exact stderr/stdout tail, "
                "and one concrete next action."
            ),
            hypothesis="Improve failure diagnosability and recovery quality.",
        ),
    ]


def backlog_candidates(backlog_path: Path, base_context: str, top: int) -> list[CandidateSpec]:
    if top <= 0:
        return []
    payload = load_json(backlog_path)
    tickets = payload.get("tickets") or []
    out: list[CandidateSpec] = []
    for idx, ticket in enumerate(tickets[:top], start=1):
        title = str(ticket.get("title", "backlog hypothesis")).strip()
        source = str(ticket.get("source", "backlog")).strip() or "backlog"
        score = float(ticket.get("score", 0.0))
        evidence = str(ticket.get("evidence", "")).strip()
        safe_slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:36] or f"backlog-{idx}"
        out.append(
            CandidateSpec(
                candidate_id=f"backlog_{idx}_{safe_slug}",
                source="backlog",
                dispatch_context=(
                    f"{base_context}\n"
                    f"Optimization hypothesis ({source}, score={score:.3f}): {title}. "
                    f"Evidence: {evidence}"
                ),
                hypothesis=f"Backlog-driven hypothesis: {title}",
            )
        )
    return out


def build_eval_cmd(repo: Path, args: argparse.Namespace, candidate: CandidateSpec) -> list[str]:
    cmd = [
        sys.executable,
        "scripts/eval_harness.py",
        "--suite",
        args.suite,
        "--config",
        args.config,
        "--athena-bin",
        args.athena_bin,
        "--output-dir",
        args.output_dir,
        "--history-file",
        args.history_file,
        "--cli-tool",
        args.cli_tool,
        "--dispatch-context",
        candidate.dispatch_context,
        "--cli-timeout-secs",
        str(args.cli_timeout_secs),
        "--worktree-ref",
        args.worktree_ref,
    ]
    if args.cli_model:
        cmd.extend(["--cli-model", args.cli_model])
    if args.max_tasks > 0:
        cmd.extend(["--max-tasks", str(args.max_tasks)])
    if args.keep_worktrees:
        cmd.append("--keep-worktrees")
    if args.no_use_worktree:
        cmd.append("--no-use-worktree")
    if args.no_cleanup_worktrees:
        cmd.append("--no-cleanup-worktrees")
    return cmd


def extract_scores(report: dict[str, Any]) -> tuple[bool, float, float, float, int]:
    gate_ok = bool(report.get("gate_ok", False))
    overall = float(report.get("overall_score", 0.0))
    rows = report.get("results") or []
    task_count = len(rows)
    if task_count <= 0:
        return gate_ok, overall, 0.0, 0.0, 0
    exec_success_rate = (
        sum(1.0 for r in rows if str(r.get("status", "")).lower() == "succeeded") / task_count
    )
    avg_task_overall = sum(float(r.get("overall", 0.0)) for r in rows) / task_count
    return gate_ok, overall, exec_success_rate, avg_task_overall, task_count


def run_candidate(repo: Path, args: argparse.Namespace, candidate: CandidateSpec) -> CandidateResult:
    cmd = build_eval_cmd(repo, args, candidate)
    if args.dry_run:
        return CandidateResult(
            candidate_id=candidate.candidate_id,
            source=candidate.source,
            hypothesis=candidate.hypothesis,
            dispatch_context=candidate.dispatch_context,
            command=cmd,
            exit_code=0,
            report_json=None,
            gate_ok=False,
            overall_score=0.0,
            exec_success_rate=0.0,
            avg_task_overall=0.0,
            task_count=0,
            error=None,
        )

    p = run_cmd(cmd, repo)
    merged_output = (p.stdout or "") + "\n" + (p.stderr or "")
    report_path_text = parse_report_json_path(merged_output)
    report = load_json(Path(report_path_text)) if report_path_text else {}
    gate_ok, overall, exec_rate, avg_task_overall, task_count = extract_scores(report)
    err = None
    if p.returncode != 0:
        err = f"eval_harness_exit={p.returncode}"
    if not report_path_text:
        err = (err + "; missing_report_json") if err else "missing_report_json"
    return CandidateResult(
        candidate_id=candidate.candidate_id,
        source=candidate.source,
        hypothesis=candidate.hypothesis,
        dispatch_context=candidate.dispatch_context,
        command=cmd,
        exit_code=p.returncode,
        report_json=report_path_text,
        gate_ok=gate_ok,
        overall_score=overall,
        exec_success_rate=exec_rate,
        avg_task_overall=avg_task_overall,
        task_count=task_count,
        error=err,
    )


def pick_winner(
    results: list[CandidateResult],
    min_improvement: float,
    max_regression: float,
) -> tuple[CandidateResult, bool, list[str], list[dict[str, Any]]]:
    baseline = next((r for r in results if r.candidate_id == "baseline"), results[0])
    gates: list[dict[str, Any]] = []
    viable: list[CandidateResult] = []
    for r in results:
        score_delta = r.overall_score - baseline.overall_score
        exec_delta = r.exec_success_rate - baseline.exec_success_rate
        gate_ok = (
            r.exit_code == 0
            and r.gate_ok
            and score_delta >= -max_regression
            and exec_delta >= -max_regression
        )
        gates.append(
            {
                "candidate_id": r.candidate_id,
                "gate_ok": gate_ok,
                "score_delta_vs_baseline": round(score_delta, 4),
                "exec_delta_vs_baseline": round(exec_delta, 4),
            }
        )
        if gate_ok:
            viable.append(r)

    if not viable:
        return baseline, False, ["no candidate passed regression gates; keep baseline"], gates

    viable.sort(
        key=lambda r: (r.overall_score, r.exec_success_rate, r.avg_task_overall),
        reverse=True,
    )
    winner = viable[0]
    promote = (
        winner.candidate_id != baseline.candidate_id
        and winner.overall_score - baseline.overall_score >= min_improvement
    )
    reasons = []
    if promote:
        reasons.append(
            f"winner improved overall_score by {winner.overall_score - baseline.overall_score:.3f}"
        )
    else:
        reasons.append("winner did not clear promotion delta threshold; keep baseline active")
    return winner, promote, reasons, gates


def render_markdown(payload: dict[str, Any]) -> str:
    lines = [
        "# Athena Optimizer Tournament",
        "",
        f"- timestamp_utc: `{payload['timestamp_utc']}`",
        f"- suite: `{payload['suite']}`",
        f"- cli_tool: `{payload['cli_tool']}`",
        f"- candidate_count: `{len(payload['candidates'])}`",
        "",
        "## Candidates",
        "",
        "| id | source | exit | gate_ok | overall | exec_success | avg_task_overall | report |",
        "|---|---|---:|---|---:|---:|---:|---|",
    ]
    for c in payload["candidates"]:
        lines.append(
            f"| `{c['candidate_id']}` | `{c['source']}` | {c['exit_code']} | "
            f"`{c['gate_ok']}` | {c['overall_score']:.3f} | {c['exec_success_rate']:.3f} | "
            f"{c['avg_task_overall']:.3f} | `{c['report_json'] or '-'}` |"
        )
    lines.extend(
        [
            "",
            "## Selection",
            "",
            f"- winner: `{payload['selection']['winner_id']}`",
            f"- promote_recommended: `{payload['selection']['promote_recommended']}`",
        ]
    )
    for reason in payload["selection"]["reasons"]:
        lines.append(f"- reason: {reason}")
    lines.append("")
    lines.append("## Regression Gates")
    for gate in payload["selection"]["regression_gates"]:
        lines.append(
            f"- `{gate['candidate_id']}` gate_ok={gate['gate_ok']} "
            f"score_delta={gate['score_delta_vs_baseline']} "
            f"exec_delta={gate['exec_delta_vs_baseline']}"
        )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    repo = Path.cwd().resolve()
    out_dir = (repo / args.output_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    maybe_regen_backlog(repo, args)

    candidates = default_candidates(args.dispatch_context_base)
    backlog_path = (repo / args.backlog_json).resolve()
    candidates.extend(backlog_candidates(backlog_path, args.dispatch_context_base, args.top_backlog))
    # Keep deterministic order and avoid overly long tournaments by default.
    dedup: dict[str, CandidateSpec] = {}
    for c in candidates:
        dedup[c.candidate_id] = c
    candidates = list(dedup.values())[: max(1, args.max_candidates)]
    if not any(c.candidate_id == "baseline" for c in candidates):
        candidates.insert(0, default_candidates(args.dispatch_context_base)[0])

    results: list[CandidateResult] = []
    for idx, c in enumerate(candidates, start=1):
        print(f"[{idx}/{len(candidates)}] candidate={c.candidate_id}", flush=True)
        result = run_candidate(repo, args, c)
        results.append(result)
        print(
            f"  exit={result.exit_code} gate_ok={result.gate_ok} "
            f"overall={result.overall_score:.3f} exec={result.exec_success_rate:.3f}",
            flush=True,
        )

    winner, promote, reasons, gates = pick_winner(
        results,
        min_improvement=args.min_improvement,
        max_regression=args.max_regression,
    )

    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    payload = {
        "timestamp_utc": ts,
        "suite": args.suite,
        "cli_tool": args.cli_tool,
        "cli_model": args.cli_model,
        "dispatch_context_base": args.dispatch_context_base,
        "min_improvement": args.min_improvement,
        "max_regression": args.max_regression,
        "dry_run": args.dry_run,
        "candidates": [asdict(r) for r in results],
        "selection": {
            "winner_id": winner.candidate_id,
            "promote_recommended": promote,
            "reasons": reasons,
            "regression_gates": gates,
        },
    }

    out_json = out_dir / f"optimizer-tournament-{ts}.json"
    out_md = out_dir / f"optimizer-tournament-{ts}.md"
    latest_json = out_dir / "optimizer-tournament-latest.json"
    latest_md = out_dir / "optimizer-tournament-latest.md"
    out_json.write_text(json.dumps(payload, indent=2))
    out_md.write_text(render_markdown(payload))
    latest_json.write_text(json.dumps(payload, indent=2))
    latest_md.write_text(render_markdown(payload))

    print(f"optimizer_tournament_json={out_json}")
    print(f"optimizer_tournament_md={out_md}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
