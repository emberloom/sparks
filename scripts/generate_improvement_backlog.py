#!/usr/bin/env python3
"""
Generate a ranked self-improvement backlog from runtime failures and code hotspots.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import sqlite3
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Generate ranked Sparks self-improvement backlog.")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--db", default="")
    p.add_argument("--maint-baseline", default="docs/maintainability-baseline.json")
    p.add_argument("--history-file", default="eval/results/history.jsonl")
    p.add_argument("--out-dir", default="eval/results")
    p.add_argument("--top", type=int, default=20)
    return p.parse_args()


def parse_db_path(config_path: Path) -> Path:
    default = Path("~/.sparks/sparks.db").expanduser()
    if not config_path.exists():
        return default
    text = config_path.read_text()
    if tomllib is not None:
        data = tomllib.loads(text)
        raw = data.get("db", {}).get("path")
        if raw:
            return Path(raw).expanduser()
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


def score_ticket(impact: int, confidence: int, effort: int, risk: str) -> float:
    risk_penalty = {"low": 1.0, "medium": 0.9, "high": 0.75}.get(risk, 0.85)
    return round((impact * confidence / max(effort, 1)) * risk_penalty, 3)


def default_owner(source: str) -> str:
    if source == "maintainability_hotspot":
        return "sparks-refactor"
    if source in {"runtime_failures", "tool_usage"}:
        return "sparks-runtime"
    if source == "eval_history":
        return "sparks-eval"
    return "sparks"


def default_eta_days(effort: int) -> str:
    return {1: "0.5d", 2: "1d", 3: "2d", 4: "3d", 5: "5d"}.get(effort, "3d")


def build_ticket(
    source: str,
    title: str,
    evidence: str,
    impact: int,
    confidence: int,
    effort: int,
    risk: str,
    acceptance: list[str],
    meta: dict[str, Any] | None = None,
) -> dict[str, Any]:
    owner = (meta or {}).get("owner", default_owner(source))
    status = (meta or {}).get("status", "open")
    eta = (meta or {}).get("eta", default_eta_days(effort))
    return {
        "source": source,
        "title": title,
        "evidence": evidence,
        "impact": impact,
        "confidence": confidence,
        "effort": effort,
        "risk": risk,
        "score": score_ticket(impact, confidence, effort, risk),
        "owner": owner,
        "status": status,
        "eta": eta,
        "acceptance": acceptance,
        "meta": meta or {},
    }


def normalize_failure_error(error: str) -> str:
    lower = error.lower().strip()
    if not lower or lower == "(none)":
        return "(none)"
    if lower in {"stale_started_timeout", "stale_started"}:
        return "stale_started"
    if lower.startswith("dispatch_wait_timeout_after=") or lower == "dispatch_timeout":
        return "dispatch_timeout"
    if lower.startswith("eval_outcome_wait_timeout_after=") or lower == "outcome_wait_timeout":
        return "outcome_wait_timeout"
    if "401 unauthorized" in lower or "user not found" in lower:
        return "llm_auth_failure"
    if "dispatch_wait_channel_closed" in lower or "dispatch_channel_closed" in lower:
        return "dispatch_channel_closed"
    return re.sub(r"\s+", " ", error.strip())


def tickets_from_failures(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    rows = conn.execute(
        """
        SELECT COALESCE(error, '(none)') as error,
               CASE
                 WHEN COALESCE(finished_at, started_at) >= datetime('now', '-72 hours') THEN 1
                 ELSE 0
               END as recent_72h
        FROM autonomous_task_outcomes
        WHERE status = 'failed'
        """
    ).fetchall()
    grouped: dict[str, dict[str, Any]] = {}
    for error, recent in rows:
        key = normalize_failure_error(str(error))
        bucket = grouped.setdefault(
            key,
            {"total": 0, "recent_72h": 0, "examples": []},
        )
        bucket["total"] += 1
        bucket["recent_72h"] += int(recent or 0)
        if len(bucket["examples"]) < 3 and error not in bucket["examples"]:
            bucket["examples"].append(error)

    out: list[dict[str, Any]] = []
    ranked = sorted(
        grouped.items(),
        key=lambda kv: (kv[1]["recent_72h"], kv[1]["total"]),
        reverse=True,
    )
    for error_key, bucket in ranked[:12]:
        total = int(bucket["total"])
        recent = int(bucket["recent_72h"])
        if total <= 0:
            continue
        title = f"Reduce recurring failure: {error_key[:72]}"
        recent_ratio = recent / max(total, 1)
        out.append(
            build_ticket(
                source="runtime_failures",
                title=title,
                evidence=f"{total} failed outcomes (recent_72h={recent}) grouped_as='{error_key}'",
                impact=5 if recent >= 3 or total >= 8 else 4,
                confidence=5 if recent_ratio >= 0.6 else 4,
                effort=3 if "timeout" in error_key else 4,
                risk="medium",
                acceptance=[
                    "Reproduce with a deterministic regression scenario.",
                    "Implement fix and add regression test or benchmark assertion.",
                    "Observe >=3 consecutive benchmark runs without this error.",
                ],
                meta={
                    "error_group": error_key,
                    "count_total": total,
                    "count_recent_72h": recent,
                    "examples": bucket["examples"],
                },
            )
        )
    return out


def tickets_from_tool_usage(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    rows = conn.execute(
        """
        SELECT tool_name, invocation_count, success_count, failure_count, last_error
        FROM tool_usage
        ORDER BY failure_count DESC
        LIMIT 20
        """
    ).fetchall()
    out: list[dict[str, Any]] = []
    for tool, inv, suc, fail, last_error in rows:
        inv = int(inv or 0)
        fail = int(fail or 0)
        if inv < 3 or fail <= 0:
            continue
        rate = fail / max(inv, 1)
        if rate < 0.3:
            continue
        out.append(
            build_ticket(
                source="tool_usage",
                title=f"Harden tool reliability: {tool}",
                evidence=f"{tool}: failures={fail}, invocations={inv}, fail_rate={rate:.2f}",
                impact=5 if rate >= 0.6 else 4,
                confidence=4,
                effort=3,
                risk="low" if tool in {"grep", "glob", "file_read"} else "medium",
                acceptance=[
                    "Classify top failure modes into deterministic error codes.",
                    "Implement retry/fallback policy where safe.",
                    "Drop fail_rate below 0.30 on subsequent snapshots.",
                ],
                meta={
                    "tool": tool,
                    "fail_rate": round(rate, 3),
                    "last_error": last_error,
                    "status": "open",
                },
            )
        )
    return out


def tickets_from_maintainability(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    data = json.loads(path.read_text())
    by_file = data.get("by_file", {})
    ranked: list[tuple[float, str, dict[str, Any]]] = []
    for file_path, info in by_file.items():
        loc = int(info.get("loc", 0))
        max_fn = int(info.get("max_fn_len", 0))
        over_120 = int(info.get("fn_over_120", 0))
        pressure = (loc / 800.0) + (max_fn / 120.0) + (over_120 * 0.8)
        if pressure < 2.2:
            continue
        ranked.append((pressure, file_path, info))
    ranked.sort(reverse=True)

    out: list[dict[str, Any]] = []
    for pressure, file_path, info in ranked[:10]:
        out.append(
            build_ticket(
                source="maintainability_hotspot",
                title=f"Refactor hotspot: {file_path}",
                evidence=(
                    f"loc={info.get('loc')} max_fn_len={info.get('max_fn_len')} "
                    f"fn_over_120={info.get('fn_over_120')} pressure={pressure:.2f}"
                ),
                impact=4,
                confidence=3,
                effort=4,
                risk="medium",
                acceptance=[
                    "Reduce longest function length and split responsibilities.",
                    "Keep behavior stable with targeted tests.",
                    "No regression in maintainability baseline metrics.",
                ],
                meta={"file": file_path, "pressure": round(pressure, 2)},
            )
        )
    return out


def tickets_from_eval_history(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    if not rows:
        return []
    recent = rows[-15:]
    fail_runs = [r for r in recent if not r.get("gate_ok", False)]
    if not fail_runs:
        return []
    fail_ratio = len(fail_runs) / len(recent)
    return [
        build_ticket(
            source="eval_history",
            title="Raise benchmark gate pass rate",
            evidence=f"{len(fail_runs)}/{len(recent)} recent runs failed gate (ratio={fail_ratio:.2f})",
            impact=5,
            confidence=4,
            effort=4,
            risk="medium",
            acceptance=[
                "Increase gate pass ratio over rolling 15-run window.",
                "Reduce execution failures in delivery lane.",
                "Keep smoke matrix stable across all CLI backends.",
            ],
            meta={"failed_runs": len(fail_runs), "window": len(recent), "ratio": round(fail_ratio, 3)},
        )
    ]


def dedupe_tickets(tickets: list[dict[str, Any]]) -> list[dict[str, Any]]:
    seen: set[tuple[str, str]] = set()
    out: list[dict[str, Any]] = []
    for t in tickets:
        key = (t["source"], t["title"])
        if key in seen:
            continue
        seen.add(key)
        out.append(t)
    return out


def render_markdown(ts: str, tickets: list[dict[str, Any]]) -> str:
    lines = [
        "# Sparks Improvement Backlog",
        "",
        f"- generated_utc: {ts}",
        f"- ticket_count: {len(tickets)}",
        "",
        "| rank | score | source | risk | status | owner | eta | title | evidence |",
        "|---:|---:|---|---|---|---|---|---|---|",
    ]
    for i, t in enumerate(tickets, start=1):
        lines.append(
            f"| {i} | {t['score']:.3f} | `{t['source']}` | `{t['risk']}` | `{t.get('status','open')}` | "
            f"`{t.get('owner','sparks')}` | `{t.get('eta','n/a')}` | {t['title']} | {t['evidence']} |"
        )
    lines.append("")
    lines.append("## Acceptance Checks")
    for i, t in enumerate(tickets, start=1):
        lines.append(f"{i}. {t['title']}")
        for check in t.get("acceptance", []):
            lines.append(f"- {check}")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    repo = Path.cwd().resolve()
    config_path = (repo / args.config).resolve()
    maint_path = (repo / args.maint_baseline).resolve()
    hist_path = (repo / args.history_file).resolve()
    out_dir = (repo / args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    db_path = Path(args.db).expanduser() if args.db else parse_db_path(config_path)
    if not db_path.is_absolute():
        db_path = (repo / db_path).resolve()
    conn = sqlite3.connect(str(db_path))

    tickets: list[dict[str, Any]] = []
    tickets.extend(tickets_from_failures(conn))
    tickets.extend(tickets_from_tool_usage(conn))
    tickets.extend(tickets_from_maintainability(maint_path))
    tickets.extend(tickets_from_eval_history(hist_path))
    conn.close()

    tickets = dedupe_tickets(tickets)
    tickets.sort(key=lambda t: t["score"], reverse=True)
    tickets = tickets[: max(args.top, 1)]

    ts = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    payload = {"generated_utc": ts, "tickets": tickets}

    out_json = out_dir / f"improvement-backlog-{ts}.json"
    out_md = out_dir / f"improvement-backlog-{ts}.md"
    latest_json = out_dir / "improvement-backlog-latest.json"
    latest_md = out_dir / "improvement-backlog-latest.md"

    out_json.write_text(json.dumps(payload, indent=2))
    out_md.write_text(render_markdown(ts, tickets))
    latest_json.write_text(json.dumps(payload, indent=2))
    latest_md.write_text(render_markdown(ts, tickets))

    print(f"backlog_json={out_json}")
    print(f"backlog_md={out_md}")
    print(f"backlog_latest_json={latest_json}")
    print(f"backlog_latest_md={latest_md}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
