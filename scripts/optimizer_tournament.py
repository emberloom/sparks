#!/usr/bin/env python3
"""
Run a Phase 4 optimizer tournament across prompt/policy candidates.

This script evaluates candidate dispatch-context variants on a fixed benchmark
suite using `scripts/eval_harness.py`, then selects and promotes a winner only
when non-regression gates and positive delta requirements are satisfied.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any


@dataclass
class CandidateSpec:
    candidate_id: str
    source: str
    dispatch_context: str
    hypothesis: str
    mutation_dimensions: dict[str, str]


@dataclass
class CandidateResult:
    candidate_id: str
    source: str
    hypothesis: str
    dispatch_context: str
    mutation_dimensions: dict[str, str]
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
    p.add_argument("--top-backlog", type=int, default=4)
    p.add_argument("--backlog-mutations-per-ticket", type=int, default=3)
    p.add_argument("--max-candidates", type=int, default=12)
    p.add_argument(
        "--include-static-mutations",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Include built-in non-backlog mutation candidates.",
    )
    p.add_argument("--min-improvement", type=float, default=0.01)
    p.add_argument("--max-regression", type=float, default=0.02)
    p.add_argument(
        "--strict-promotion",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Require non-negative exec/task deltas in addition to score delta for promotion.",
    )
    p.add_argument("--active-profile-json", default="eval/results/optimizer-profile.json")
    p.add_argument(
        "--promote-profile",
        action="store_true",
        help="Persist winner context to active profile when gates + positive delta pass.",
    )
    p.add_argument(
        "--fail-on-baseline-gate",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Return non-zero when baseline candidate fails real gate.",
    )
    p.add_argument("--dry-run", action="store_true")
    return p.parse_args()


def compact_text(text: str, max_chars: int) -> str:
    value = re.sub(r"\s+", " ", text.strip())
    if len(value) <= max_chars:
        return value
    return value[:max_chars].rstrip()


def slugify(text: str, max_chars: int = 40) -> str:
    out = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    if not out:
        out = "candidate"
    return out[:max_chars].strip("-")


def kv_from_output(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        key = k.strip()
        value = v.strip()
        if key:
            out[key] = value
    return out


def parse_report_json_path(output: str) -> str | None:
    kv = kv_from_output(output)
    if "report_json" in kv:
        return kv["report_json"]
    matches = re.findall(r"report_json=(.+)", output)
    if not matches:
        return None
    return matches[-1].strip()


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


def load_active_profile(profile_path: Path, fallback_context: str) -> dict[str, Any]:
    profile = {
        "profile_id": "baseline",
        "dispatch_context": fallback_context,
        "source": "dispatch_context_base",
        "updated_at_utc": None,
    }
    payload = load_json(profile_path)
    context = payload.get("dispatch_context")
    if isinstance(context, str) and context.strip():
        profile.update(
            {
                "profile_id": str(payload.get("profile_id") or "baseline"),
                "dispatch_context": context,
                "source": str(payload.get("source") or "optimizer_profile"),
                "updated_at_utc": payload.get("updated_at_utc"),
            }
        )
    return profile


def baseline_candidate_from_profile(profile: dict[str, Any]) -> CandidateSpec:
    profile_id = str(profile.get("profile_id") or "baseline")
    source = str(profile.get("source") or "baseline")
    updated = profile.get("updated_at_utc")
    hypothesis = f"Control candidate from active profile '{profile_id}'"
    if updated:
        hypothesis += f" (updated {updated})"
    return CandidateSpec(
        candidate_id="baseline",
        source=f"baseline:{source}",
        dispatch_context=str(profile.get("dispatch_context") or ""),
        hypothesis=hypothesis,
        mutation_dimensions={
            "constraint_strictness": "baseline",
            "soul_composition": "baseline",
            "mutation_strategy": "baseline",
        },
    )


def static_mutation_candidates(base_context: str) -> list[CandidateSpec]:
    raw: list[tuple[str, str, str, str]] = [
        (
            "mutation_plan_verify",
            "plan_verify",
            "Execution contract: return PLAN and EXECUTION sections. Run required verification commands before final completion.",
            "Improve structure and verification compliance.",
        ),
        (
            "mutation_minimal_diff",
            "minimal_diff",
            "Change policy: prefer smallest safe diff, preserve existing architecture, and avoid unrelated edits.",
            "Reduce noisy diffs and improve diff-quality scores.",
        ),
        (
            "mutation_blocker_evidence",
            "blocker_evidence",
            "If blocked, report deterministic evidence: command, exact stderr/stdout tail, and one concrete next action.",
            "Improve failure diagnosability and recovery quality.",
        ),
    ]
    out: list[CandidateSpec] = []
    for idx, (candidate_id, mutation_id, mutation_text, hypothesis) in enumerate(raw):
        constraint_level = CONSTRAINT_STRICTNESS_LIBRARY[idx % len(CONSTRAINT_STRICTNESS_LIBRARY)][
            0
        ]
        soul_level = SOUL_COMPOSITION_LIBRARY[idx % len(SOUL_COMPOSITION_LIBRARY)][0]
        context = apply_mutation_axes(
            base_context,
            mutation_text,
            constraint_level,
            soul_level,
        )
        out.append(
            CandidateSpec(
                candidate_id=candidate_id,
                source="mutation_static",
                dispatch_context=context,
                hypothesis=hypothesis,
                mutation_dimensions={
                    "mutation_strategy": mutation_id,
                    "constraint_strictness": constraint_level,
                    "soul_composition": soul_level,
                },
            )
        )
    return out


def source_focus_hint(source: str) -> str:
    source_key = source.strip().lower()
    if source_key == "runtime_failures":
        return (
            "Focus on deterministic failure reproduction, error taxonomy, and safe retry/fallback "
            "logic with regression checks."
        )
    if source_key == "tool_usage":
        return "Focus on tool contract reliability, explicit error codes, and fallback behavior."
    if source_key == "maintainability_hotspot":
        return "Focus on decomposition: smaller functions, scoped patches, and behavior-preserving refactors."
    if source_key == "eval_history":
        return "Focus on improving gate pass outcomes without sacrificing verification rigor."
    return "Focus on measurable benchmark and safety improvements."


CONSTRAINT_STRICTNESS_LIBRARY: list[tuple[str, str]] = [
    (
        "strict",
        "Treat all declared constraints as mandatory and block completion when evidence is missing.",
    ),
    (
        "balanced",
        "Treat safety constraints as mandatory; optimize non-critical constraints when it improves verified outcomes.",
    ),
    (
        "adaptive",
        "Adapt non-critical constraint verbosity to task risk while preserving strict safety and verification floors.",
    ),
]

SOUL_COMPOSITION_LIBRARY: list[tuple[str, str]] = [
    (
        "minimal",
        "Use concise persona guidance: high signal, low verbosity, execution-first.",
    ),
    (
        "balanced",
        "Blend persona tone with operational rigor and explicit verification intent.",
    ),
    (
        "context_rich",
        "Use richer persona framing plus explicit mission/risk framing for complex tasks.",
    ),
]


def validate_mutation_axes(constraint_level: str, soul_level: str) -> None:
    allowed_constraints = {name for name, _ in CONSTRAINT_STRICTNESS_LIBRARY}
    allowed_souls = {name for name, _ in SOUL_COMPOSITION_LIBRARY}
    if constraint_level not in allowed_constraints:
        raise ValueError(f"unsupported constraint strictness axis: {constraint_level}")
    if soul_level not in allowed_souls:
        raise ValueError(f"unsupported soul composition axis: {soul_level}")


def apply_mutation_axes(
    base_context: str,
    mutation_text: str,
    constraint_level: str,
    soul_level: str,
) -> str:
    validate_mutation_axes(constraint_level, soul_level)
    constraint_text = dict(CONSTRAINT_STRICTNESS_LIBRARY)[constraint_level]
    soul_text = dict(SOUL_COMPOSITION_LIBRARY)[soul_level]
    return (
        f"{base_context}\n"
        f"Mutation strategy: {mutation_text}\n"
        f"Constraint strictness ({constraint_level}): {constraint_text}\n"
        f"Soul composition ({soul_level}): {soul_text}\n"
        "Safety floor: never bypass destructive-command, credential, or verification guardrails."
    )


BACKLOG_MUTATION_LIBRARY: list[tuple[str, str]] = [
    (
        "ticket_focus",
        "Prioritize the ticket hypothesis directly. Implement only the minimum patch required to move this KPI.",
    ),
    (
        "acceptance_guarded",
        "Treat acceptance checks as hard constraints. Do not claim completion unless each acceptance bullet is evidenced.",
    ),
    (
        "deterministic_diagnostics",
        "When blocked, output deterministic diagnostics: failed command, exact error token, and next repair action.",
    ),
    (
        "regression_first",
        "Add or run the smallest regression check that proves the target failure class is reduced.",
    ),
]


def backlog_candidates(
    backlog_path: Path,
    base_context: str,
    top: int,
    mutations_per_ticket: int,
) -> list[CandidateSpec]:
    if top <= 0 or mutations_per_ticket <= 0:
        return []
    payload = load_json(backlog_path)
    tickets = payload.get("tickets") or []
    out: list[CandidateSpec] = []
    for idx, ticket in enumerate(tickets[:top], start=1):
        title = compact_text(str(ticket.get("title", "backlog hypothesis")), 120)
        source = compact_text(str(ticket.get("source", "backlog")), 48)
        risk = compact_text(str(ticket.get("risk", "unknown")), 16)
        score = float(ticket.get("score", 0.0))
        evidence = compact_text(str(ticket.get("evidence", "")), 220)
        acceptance = ticket.get("acceptance") or []
        acceptance_focus = " | ".join(compact_text(str(x), 100) for x in acceptance[:2]) or "-"
        title_slug = slugify(title, 28)
        focus = source_focus_hint(source)
        count = min(mutations_per_ticket, len(BACKLOG_MUTATION_LIBRARY))
        for m_idx, (mutation_id, mutation_text) in enumerate(BACKLOG_MUTATION_LIBRARY[:count], start=1):
            candidate_id = f"backlog_{idx}_{m_idx}_{mutation_id}_{title_slug}"
            hypothesis = f"Backlog hypothesis ({source}): {title}"
            constraint_level = CONSTRAINT_STRICTNESS_LIBRARY[(idx + m_idx - 2) % len(CONSTRAINT_STRICTNESS_LIBRARY)][0]
            soul_level = SOUL_COMPOSITION_LIBRARY[(idx * m_idx - 1) % len(SOUL_COMPOSITION_LIBRARY)][0]
            base_ticket_context = (
                f"{base_context}\n"
                f"Backlog ticket source={source} risk={risk} score={score:.3f}: {title}\n"
                f"Evidence: {evidence}\n"
                f"Acceptance focus: {acceptance_focus}\n"
                f"Source focus: {focus}"
            )
            context = apply_mutation_axes(
                base_ticket_context,
                f"Backlog mutation ({mutation_id}): {mutation_text}",
                constraint_level,
                soul_level,
            )
            out.append(
                CandidateSpec(
                    candidate_id=candidate_id,
                    source=f"backlog:{source}",
                    dispatch_context=context,
                    hypothesis=hypothesis,
                    mutation_dimensions={
                        "mutation_strategy": mutation_id,
                        "constraint_strictness": constraint_level,
                        "soul_composition": soul_level,
                    },
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
            mutation_dimensions=dict(candidate.mutation_dimensions),
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
        mutation_dimensions=dict(candidate.mutation_dimensions),
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
    strict_promotion: bool,
) -> tuple[CandidateResult, bool, list[str], list[dict[str, Any]]]:
    baseline = next((r for r in results if r.candidate_id == "baseline"), results[0])
    gates: list[dict[str, Any]] = []
    viable: list[CandidateResult] = []
    for r in results:
        score_delta = r.overall_score - baseline.overall_score
        exec_delta = r.exec_success_rate - baseline.exec_success_rate
        avg_delta = r.avg_task_overall - baseline.avg_task_overall
        gate_ok = (
            r.exit_code == 0
            and r.gate_ok
            and score_delta >= -max_regression
            and exec_delta >= -max_regression
            and avg_delta >= -max_regression
        )
        gates.append(
            {
                "candidate_id": r.candidate_id,
                "gate_ok": gate_ok,
                "score_delta_vs_baseline": round(score_delta, 4),
                "exec_delta_vs_baseline": round(exec_delta, 4),
                "avg_task_delta_vs_baseline": round(avg_delta, 4),
            }
        )
        if gate_ok:
            viable.append(r)

    if not viable:
        return baseline, False, ["no candidate passed non-regression gates; keep baseline"], gates

    viable.sort(
        key=lambda r: (r.overall_score, r.exec_success_rate, r.avg_task_overall),
        reverse=True,
    )
    winner = viable[0]
    score_delta = winner.overall_score - baseline.overall_score
    exec_delta = winner.exec_success_rate - baseline.exec_success_rate
    avg_delta = winner.avg_task_overall - baseline.avg_task_overall

    promote = winner.candidate_id != baseline.candidate_id and score_delta >= min_improvement
    reasons = []
    if promote and strict_promotion and (exec_delta < 0.0 or avg_delta < 0.0):
        promote = False
        reasons.append(
            "strict promotion blocked: non-positive execution/task delta despite score improvement"
        )
    if promote:
        reasons.append(
            f"winner improved overall_score by {score_delta:.3f} with non-regression gates passing"
        )
    else:
        reasons.append("winner did not satisfy promotion thresholds; keep baseline active")
    return winner, promote, reasons, gates


def write_active_profile(
    profile_path: Path,
    winner: CandidateResult,
    suite: str,
    cli_tool: str,
    ts: str,
    baseline: CandidateResult,
    tournament_json: Path,
) -> dict[str, Any]:
    profile = {
        "profile_id": winner.candidate_id,
        "source": "optimizer_tournament",
        "dispatch_context": winner.dispatch_context,
        "hypothesis": winner.hypothesis,
        "suite": suite,
        "cli_tool": cli_tool,
        "updated_at_utc": ts,
        "winner_metrics": {
            "overall_score": winner.overall_score,
            "exec_success_rate": winner.exec_success_rate,
            "avg_task_overall": winner.avg_task_overall,
        },
        "baseline_metrics": {
            "overall_score": baseline.overall_score,
            "exec_success_rate": baseline.exec_success_rate,
            "avg_task_overall": baseline.avg_task_overall,
        },
        "tournament_json": str(tournament_json),
    }
    profile_path.parent.mkdir(parents=True, exist_ok=True)
    profile_path.write_text(json.dumps(profile, indent=2))
    return profile


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
            f"exec_delta={gate['exec_delta_vs_baseline']} "
            f"avg_task_delta={gate['avg_task_delta_vs_baseline']}"
        )
    lines.append("")
    lines.append("## Promotion Execution")
    lines.append(f"- enabled: `{payload['promotion_execution']['enabled']}`")
    lines.append(f"- status: `{payload['promotion_execution']['status']}`")
    for reason in payload["promotion_execution"]["reasons"]:
        lines.append(f"- reason: {reason}")
    lines.append(
        f"- active_profile_json: `{payload['promotion_execution'].get('active_profile_json', '-')}`"
    )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    repo = Path.cwd().resolve()
    out_dir = (repo / args.output_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    maybe_regen_backlog(repo, args)

    active_profile_path = (repo / args.active_profile_json).resolve()
    active_profile_before = load_active_profile(active_profile_path, args.dispatch_context_base)
    seed_context = str(active_profile_before.get("dispatch_context") or args.dispatch_context_base)

    candidates: list[CandidateSpec] = [baseline_candidate_from_profile(active_profile_before)]
    if args.include_static_mutations:
        candidates.extend(static_mutation_candidates(seed_context))
    backlog_path = (repo / args.backlog_json).resolve()
    candidates.extend(
        backlog_candidates(
            backlog_path,
            seed_context,
            top=args.top_backlog,
            mutations_per_ticket=args.backlog_mutations_per_ticket,
        )
    )

    # Keep deterministic order, dedupe by candidate id, and reserve one slot for baseline.
    dedup: dict[str, CandidateSpec] = {}
    for c in candidates:
        dedup[c.candidate_id] = c
    baseline = dedup.get("baseline") or candidates[0]
    non_baseline = [c for cid, c in dedup.items() if cid != "baseline"]
    max_total = max(1, args.max_candidates)
    candidates = [baseline] + non_baseline[: max_total - 1]

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
        strict_promotion=args.strict_promotion,
    )
    baseline_result = next((r for r in results if r.candidate_id == "baseline"), results[0])

    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    out_json = out_dir / f"optimizer-tournament-{ts}.json"
    out_md = out_dir / f"optimizer-tournament-{ts}.md"
    latest_json = out_dir / "optimizer-tournament-latest.json"
    latest_md = out_dir / "optimizer-tournament-latest.md"

    promotion_execution = {
        "enabled": bool(args.promote_profile),
        "status": "disabled",
        "reasons": [],
        "active_profile_json": str(active_profile_path),
    }
    active_profile_after = active_profile_before
    if args.promote_profile:
        if args.dry_run:
            promotion_execution["status"] = "skipped_dry_run"
            promotion_execution["reasons"].append("dry-run mode: profile not written")
        elif promote:
            active_profile_after = write_active_profile(
                active_profile_path,
                winner,
                suite=args.suite,
                cli_tool=args.cli_tool,
                ts=ts,
                baseline=baseline_result,
                tournament_json=out_json,
            )
            promotion_execution["status"] = "promoted"
            promotion_execution["reasons"].append("winner passed non-regression gates and positive delta")
        else:
            promotion_execution["status"] = "not_promoted"
            promotion_execution["reasons"].append(
                "promotion criteria not met (non-regression gates and/or positive delta)"
            )

    payload = {
        "timestamp_utc": ts,
        "suite": args.suite,
        "cli_tool": args.cli_tool,
        "cli_model": args.cli_model,
        "dispatch_context_base": args.dispatch_context_base,
        "active_profile_before": active_profile_before,
        "active_profile_after": active_profile_after,
        "min_improvement": args.min_improvement,
        "max_regression": args.max_regression,
        "strict_promotion": args.strict_promotion,
        "dry_run": args.dry_run,
        "candidates": [asdict(r) for r in results],
        "selection": {
            "winner_id": winner.candidate_id,
            "promote_recommended": promote,
            "reasons": reasons,
            "regression_gates": gates,
        },
        "promotion_execution": promotion_execution,
    }

    out_json.write_text(json.dumps(payload, indent=2))
    out_md.write_text(render_markdown(payload))
    latest_json.write_text(json.dumps(payload, indent=2))
    latest_md.write_text(render_markdown(payload))

    print(f"optimizer_tournament_json={out_json}")
    print(f"optimizer_tournament_md={out_md}")

    if args.fail_on_baseline_gate and not baseline_result.gate_ok:
        print("baseline_gate_ok=false", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
