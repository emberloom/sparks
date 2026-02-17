#!/usr/bin/env python3
"""
Run a supervised batch of `athena self-build run` jobs and aggregate artifacts.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass
class BatchRow:
    index: int
    ticket: str
    exit_code: int
    run_id: str | None
    self_build_json: str | None
    self_build_md: str | None
    review_json: str | None
    review_md: str | None
    promotion_status: str | None
    policy_line: str | None
    critic_score: float | None
    guardrails_passed: bool | None
    pr_url: str | None


def load_tickets(path: Path) -> list[str]:
    lines = path.read_text().splitlines()
    tickets: list[str] = []
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        tickets.append(line)
    return tickets


def kv_from_output(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in text.splitlines():
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        key = k.strip()
        val = v.strip()
        if key and key.startswith("self_build_"):
            out[key] = val
    return out


def run_cmd(cmd: list[str], cwd: Path, timeout: int) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout,
    )


def read_json(path_text: str | None) -> dict[str, Any]:
    if not path_text:
        return {}
    p = Path(path_text)
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text())
    except Exception:
        return {}


def write_report(repo: Path, rows: list[BatchRow], args: argparse.Namespace) -> tuple[Path, Path]:
    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    out_dir = repo / "eval" / "results"
    out_dir.mkdir(parents=True, exist_ok=True)
    base = f"self-build-batch-{ts}"
    json_path = out_dir / f"{base}.json"
    md_path = out_dir / f"{base}.md"
    latest_json = out_dir / "self-build-batch-latest.json"
    latest_md = out_dir / "self-build-batch-latest.md"

    payload = {
        "timestamp_utc": ts,
        "tickets_file": str(args.tickets_file),
        "risk": args.risk,
        "promote_mode": args.promote_mode,
        "base_branch": args.base_branch,
        "cli_tool": args.cli_tool,
        "maintenance_profile": args.maintenance_profile,
        "dry_run": args.dry_run,
        "rows": [r.__dict__ for r in rows],
        "summary": {
            "runs": len(rows),
            "succeeded": sum(1 for r in rows if r.exit_code == 0),
            "failed": sum(1 for r in rows if r.exit_code != 0),
            "guardrail_failures": sum(1 for r in rows if r.guardrails_passed is False),
            "prs_opened": sum(1 for r in rows if (r.pr_url or "").startswith("http")),
        },
    }
    json_path.write_text(json.dumps(payload, indent=2))

    lines = [
        "# Self-Build Supervised Batch",
        "",
        f"- timestamp_utc: `{ts}`",
        f"- tickets_file: `{args.tickets_file}`",
        f"- risk: `{args.risk}`",
        f"- promote_mode: `{args.promote_mode}`",
        f"- base_branch: `{args.base_branch}`",
        f"- cli_tool: `{args.cli_tool or '-'}'",
        f"- maintenance_profile: `{args.maintenance_profile}`",
        f"- dry_run: `{args.dry_run}`",
        "",
        "## Runs",
        "",
        "| # | exit | run_id | promotion_status | critic_score | guardrails_passed | pr_url |",
        "|---|---|---|---|---:|---|---|",
    ]
    for r in rows:
        lines.append(
            f"| {r.index} | {r.exit_code} | `{r.run_id or '-'}` | `{r.promotion_status or '-'}` | "
            f"{(f'{r.critic_score:.2f}' if r.critic_score is not None else '-')} | "
            f"`{r.guardrails_passed}` | `{r.pr_url or '-'}` |"
        )
    md_path.write_text("\n".join(lines) + "\n")

    latest_json.write_text(json_path.read_text())
    latest_md.write_text(md_path.read_text())
    return json_path, md_path


def main() -> int:
    p = argparse.ArgumentParser(description="Run supervised self-build batch")
    p.add_argument("--tickets-file", required=True, type=Path)
    p.add_argument("--risk", default="low")
    p.add_argument("--wait-secs", type=int, default=300)
    p.add_argument("--cli-tool", default=None)
    p.add_argument("--cli-model", default=None)
    p.add_argument("--maintenance-profile", default="rust", choices=["rust", "generic"])
    p.add_argument("--promote-mode", default="pr", choices=["none", "pr", "auto"])
    p.add_argument("--base-branch", default="main")
    p.add_argument("--allow-auto-promote", action="store_true")
    p.add_argument("--max-runs", type=int, default=0)
    p.add_argument("--timeout-secs", type=int, default=1800)
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--athena-bin", default="./target/debug/athena")
    args = p.parse_args()

    repo = Path(__file__).resolve().parents[1]
    tickets = load_tickets(args.tickets_file)
    if args.max_runs > 0:
        tickets = tickets[: args.max_runs]
    if not tickets:
        print("No tickets found.")
        return 1

    rows: list[BatchRow] = []
    for i, ticket in enumerate(tickets, start=1):
        cmd = [
            args.athena_bin,
            "self-build",
            "run",
            "--ticket",
            ticket,
            "--risk",
            args.risk,
            "--wait-secs",
            str(args.wait_secs),
            "--maintenance-profile",
            args.maintenance_profile,
            "--promote-mode",
            args.promote_mode,
            "--base-branch",
            args.base_branch,
        ]
        if args.allow_auto_promote:
            cmd.append("--allow-auto-promote")
        if args.cli_tool:
            cmd.extend(["--cli-tool", args.cli_tool])
        if args.cli_model:
            cmd.extend(["--cli-model", args.cli_model])

        printable = " ".join(shlex.quote(x) for x in cmd)
        print(f"[{i}/{len(tickets)}] {printable}", flush=True)
        if args.dry_run:
            rows.append(
                BatchRow(
                    index=i,
                    ticket=ticket,
                    exit_code=0,
                    run_id=None,
                    self_build_json=None,
                    self_build_md=None,
                    review_json=None,
                    review_md=None,
                    promotion_status="dry_run",
                    policy_line="dry_run",
                    critic_score=None,
                    guardrails_passed=None,
                    pr_url=None,
                )
            )
            continue

        proc = run_cmd(cmd, repo, timeout=args.timeout_secs)
        merged_out = (proc.stdout or "") + "\n" + (proc.stderr or "")
        kv = kv_from_output(merged_out)
        payload = read_json(kv.get("self_build_json"))
        promotion_exec = payload.get("promotion_execution", {})
        row = BatchRow(
            index=i,
            ticket=ticket,
            exit_code=proc.returncode,
            run_id=kv.get("self_build_run_id"),
            self_build_json=kv.get("self_build_json"),
            self_build_md=kv.get("self_build_md"),
            review_json=kv.get("self_build_review_json"),
            review_md=kv.get("self_build_review_md"),
            promotion_status=kv.get("self_build_promotion_status")
            or promotion_exec.get("status"),
            policy_line=next(
                (ln for ln in merged_out.splitlines() if ln.startswith("self_build_policy ")),
                None,
            ),
            critic_score=(
                float(kv["self_build_critic_score"])
                if "self_build_critic_score" in kv
                else None
            ),
            guardrails_passed=(
                kv.get("self_build_guardrails_passed", "").lower() == "true"
                if "self_build_guardrails_passed" in kv
                else None
            ),
            pr_url=kv.get("self_build_pr_url") or promotion_exec.get("pr_url"),
        )
        rows.append(row)
        print(
            f"  exit={row.exit_code} run_id={row.run_id or '-'} promotion={row.promotion_status or '-'}",
            flush=True,
        )

    report_json, report_md = write_report(repo, rows, args)
    print(f"batch_report_json={report_json}")
    print(f"batch_report_md={report_md}")
    return 0 if all(r.exit_code == 0 for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())

