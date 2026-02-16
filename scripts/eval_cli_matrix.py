#!/usr/bin/env python3
"""
Run eval harness across multiple coding CLIs and produce a comparison summary.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Run Athena eval harness matrix across coding CLIs.")
    p.add_argument("--tools", default="claude_code,codex,opencode")
    p.add_argument("--suite", default="eval/benchmark-cli-smoke.json")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--athena-bin", default="target/debug/athena")
    p.add_argument("--output-dir", default="eval/results")
    p.add_argument("--history-file", default="eval/results/history.jsonl")
    p.add_argument("--dispatch-context", default="[benchmark_fast_cli]")
    p.add_argument("--cli-timeout-secs", type=int, default=120)
    p.add_argument("--max-tasks", type=int, default=0)
    p.add_argument("--worktree-ref", default="HEAD")
    p.add_argument("--keep-worktrees", action="store_true")
    p.add_argument("--no-use-worktree", action="store_true")
    p.add_argument("--no-cleanup-worktrees", action="store_true")
    return p.parse_args()


def parse_report_path(stdout: str) -> Path | None:
    m = re.search(r"report_json=(.+)", stdout)
    if not m:
        return None
    return Path(m.group(1).strip())


def run_harness_for_tool(args: argparse.Namespace, tool: str) -> dict[str, Any]:
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
        tool,
        "--dispatch-context",
        args.dispatch_context,
        "--cli-timeout-secs",
        str(args.cli_timeout_secs),
        "--worktree-ref",
        args.worktree_ref,
    ]
    if args.max_tasks > 0:
        cmd.extend(["--max-tasks", str(args.max_tasks)])
    if args.keep_worktrees:
        cmd.append("--keep-worktrees")
    if args.no_use_worktree:
        cmd.append("--no-use-worktree")
    if args.no_cleanup_worktrees:
        cmd.append("--no-cleanup-worktrees")

    print(f"[matrix] running tool={tool}", flush=True)
    p = subprocess.run(
        cmd,
        text=True,
        capture_output=True,
        check=False,
    )

    if p.stdout:
        print(p.stdout, end="" if p.stdout.endswith("\n") else "\n", flush=True)
    if p.stderr:
        print(p.stderr, end="" if p.stderr.endswith("\n") else "\n", file=sys.stderr, flush=True)

    report_path = parse_report_path(p.stdout)
    report: dict[str, Any] = {}
    if report_path and report_path.exists():
        report = json.loads(report_path.read_text())

    return {
        "tool": tool,
        "exit_code": p.returncode,
        "report_json": str(report_path) if report_path else None,
        "report": report,
    }


def write_summary(output_dir: Path, results: list[dict[str, Any]]) -> tuple[Path, Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    out_json = output_dir / f"cli-matrix-{ts}.json"
    out_md = output_dir / f"cli-matrix-{ts}.md"

    out_json.write_text(json.dumps({"timestamp_utc": ts, "results": results}, indent=2))

    lines = [
        "# Athena CLI Matrix",
        "",
        f"- timestamp_utc: {ts}",
        "",
        "| cli_tool | harness_exit | gate | overall | exec_success_rate |",
        "|---|---:|---|---:|---:|",
    ]
    for item in results:
        report = item.get("report") or {}
        gate = report.get("gate_ok")
        status = "PASS" if gate is True else "FAIL" if gate is False else "n/a"
        rows = report.get("results") or []
        exec_rate = (
            sum(1.0 for r in rows if r.get("status") == "succeeded") / max(len(rows), 1)
            if rows
            else 0.0
        )
        lines.append(
            f"| `{item['tool']}` | {item['exit_code']} | {status} | "
            f"{float(report.get('overall_score', 0.0)):.2f} | {exec_rate:.2f} |"
        )
    lines.append("")
    lines.append("## Reports")
    for item in results:
        lines.append(f"- `{item['tool']}`: {item.get('report_json') or 'missing'}")

    out_md.write_text("\n".join(lines) + "\n")
    return out_json, out_md


def main() -> int:
    args = parse_args()
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    if not tools:
        print("No tools specified.", file=sys.stderr)
        return 2

    results: list[dict[str, Any]] = []
    for tool in tools:
        results.append(run_harness_for_tool(args, tool))

    output_dir = Path(args.output_dir)
    out_json, out_md = write_summary(output_dir, results)
    print(f"matrix_json={out_json}")
    print(f"matrix_md={out_md}")

    all_ran = all(item.get("report_json") for item in results)
    return 0 if all_ran else 1


if __name__ == "__main__":
    raise SystemExit(main())
