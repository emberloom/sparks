#!/usr/bin/env python3
"""
Combined KPI + eval dashboard.

Reads eval history (JSONL) and autonomous KPI data from SQLite,
then renders a compact markdown dashboard.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sqlite3
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


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


def load_history(path: Path, limit: int) -> list[dict]:
    if not path.exists():
        return []
    rows: list[dict] = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return rows[-limit:]


def pct(n: int, d: int) -> str:
    if d <= 0:
        return "n/a"
    return f"{(100.0 * n / d):.1f}%"


def parse_sqlite_ts(raw: str | None) -> dt.datetime | None:
    if not raw:
        return None
    try:
        return dt.datetime.strptime(raw, "%Y-%m-%d %H:%M:%S")
    except ValueError:
        return None


def format_mttf(seconds: float | None) -> str:
    if seconds is None:
        return "n/a"
    minutes = seconds / 60.0
    if minutes < 60:
        return f"{minutes:.1f}m"
    return f"{(minutes / 60.0):.2f}h"


def compute_mttf_by_lane_risk(conn: sqlite3.Connection, repo_name: str) -> dict[tuple[str, str], float]:
    rows = conn.execute(
        """
        SELECT lane, risk_tier, status, COALESCE(finished_at, started_at) AS event_time
        FROM autonomous_task_outcomes
        WHERE repo = ?
          AND status IN ('failed', 'succeeded')
        ORDER BY lane, risk_tier, event_time
        """,
        (repo_name,),
    ).fetchall()

    pending_failure: dict[tuple[str, str], dt.datetime] = {}
    deltas: dict[tuple[str, str], list[float]] = {}

    for lane, risk, status, event_time_raw in rows:
        key = (str(lane), str(risk))
        event_time = parse_sqlite_ts(str(event_time_raw))
        if event_time is None:
            continue

        if status == "failed":
            pending_failure.setdefault(key, event_time)
            continue

        if status == "succeeded" and key in pending_failure:
            delta = (event_time - pending_failure[key]).total_seconds()
            if delta >= 0:
                deltas.setdefault(key, []).append(delta)
            pending_failure.pop(key, None)

    out: dict[tuple[str, str], float] = {}
    for key, values in deltas.items():
        if values:
            out[key] = sum(values) / len(values)
    return out


def query_kpis(conn: sqlite3.Connection, repo_name: str) -> list[dict]:
    mttf_by_key = compute_mttf_by_lane_risk(conn, repo_name)
    rows = conn.execute(
        """
        SELECT lane, risk_tier,
               COALESCE(SUM(CASE WHEN status IN ('succeeded','failed','rolled_back') THEN 1 ELSE 0 END), 0) started,
               COALESCE(SUM(CASE WHEN status='succeeded' THEN 1 ELSE 0 END), 0) succeeded,
               COALESCE(SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END), 0) failed,
               COALESCE(SUM(verification_total), 0) ver_total,
               COALESCE(SUM(verification_passed), 0) ver_passed,
               COALESCE(SUM(rolled_back), 0) rollbacks
        FROM autonomous_task_outcomes
        WHERE repo = ?
        GROUP BY lane, risk_tier
        ORDER BY lane, risk_tier
        """,
        (repo_name,),
    ).fetchall()
    out: list[dict] = []
    for r in rows:
        lane = r[0]
        risk = r[1]
        started = int(r[2] or 0)
        succeeded = int(r[3] or 0)
        failed = int(r[4] or 0)
        ver_total = int(r[5] or 0)
        ver_passed = int(r[6] or 0)
        rollbacks = int(r[7] or 0)
        key = (str(lane), str(risk))
        mttf_seconds = mttf_by_key.get(key)
        out.append(
            {
                "lane": lane,
                "risk": risk,
                "started": started,
                "succeeded": succeeded,
                "failed": failed,
                "ver_total": ver_total,
                "ver_passed": ver_passed,
                "rollbacks": rollbacks,
                "task_success_rate": pct(succeeded, started),
                "verification_pass_rate": pct(ver_passed, ver_total),
                "rollback_rate": pct(rollbacks, max(succeeded, 1)),
                "mean_time_to_fix": format_mttf(mttf_seconds),
            }
        )
    return out


def render_dashboard(history: list[dict], kpis: list[dict], repo_name: str) -> str:
    now = dt.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    lines: list[str] = []
    lines.append("# Athena KPI + Eval Dashboard")
    lines.append("")
    lines.append(f"- generated_utc: {now}")
    lines.append(f"- repo: `{repo_name}`")
    lines.append("")

    lines.append("## Latest Eval")
    if history:
        h = history[-1]
        lines.append(f"- suite: `{h.get('suite', 'unknown')}`")
        lines.append(f"- timestamp_utc: `{h.get('timestamp_utc', 'unknown')}`")
        lines.append(f"- gate: `{'PASS' if h.get('gate_ok') else 'FAIL'}`")
        lines.append(f"- overall_score: `{h.get('overall_score', 0):.2f}`")
        lines.append(f"- threshold: `{h.get('threshold', 0):.2f}`")
        lines.append(f"- task_count: `{h.get('task_count', 0)}`")
        lines.append(f"- exec_success_rate: `{h.get('exec_success_rate', 0):.2f}`")
    else:
        lines.append("- no eval history found")
    lines.append("")

    lines.append("## Eval Trend")
    lines.append("")
    lines.append("| timestamp | suite | gate | overall | tasks | exec_success_rate |")
    lines.append("|---|---|---|---:|---:|---:|")
    if history:
        for h in history:
            lines.append(
                f"| `{h.get('timestamp_utc','?')}` | `{h.get('suite','?')}` | "
                f"`{'PASS' if h.get('gate_ok') else 'FAIL'}` | "
                f"{float(h.get('overall_score', 0)):.2f} | "
                f"{int(h.get('task_count', 0))} | "
                f"{float(h.get('exec_success_rate', 0)):.2f} |"
            )
    else:
        lines.append("| - | - | - | - | - | - |")
    lines.append("")

    lines.append("## Current KPI Snapshot (from outcomes)")
    lines.append("")
    lines.append("| lane | risk | started | succeeded | failed | task_success | verification | rollback | mttf |")
    lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|")
    if kpis:
        for r in kpis:
            lines.append(
                f"| `{r['lane']}` | `{r['risk']}` | {r['started']} | {r['succeeded']} | {r['failed']} | "
                f"{r['task_success_rate']} | {r['verification_pass_rate']} | {r['rollback_rate']} | {r['mean_time_to_fix']} |"
            )
    else:
        lines.append("| - | - | 0 | 0 | 0 | n/a | n/a | n/a | n/a |")
    lines.append("")
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Render combined KPI + eval dashboard.")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--repo", default="athena")
    p.add_argument("--history-file", default="eval/results/history.jsonl")
    p.add_argument("--history-limit", type=int, default=20)
    p.add_argument("--out-file", default="eval/results/dashboard.md")
    return p.parse_args()


def resolve(repo: Path, p: str) -> Path:
    x = Path(p)
    return x if x.is_absolute() else (repo / x).resolve()


def main() -> int:
    args = parse_args()
    repo = Path.cwd().resolve()
    config_path = resolve(repo, args.config)
    history_path = resolve(repo, args.history_file)
    out_file = resolve(repo, args.out_file)

    db_path = parse_db_path(config_path)
    conn = sqlite3.connect(str(db_path))
    history = load_history(history_path, args.history_limit)
    kpis = query_kpis(conn, args.repo)
    conn.close()

    content = render_dashboard(history, kpis, args.repo)
    out_file.parent.mkdir(parents=True, exist_ok=True)
    out_file.write_text(content)
    print(f"dashboard={out_file}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
