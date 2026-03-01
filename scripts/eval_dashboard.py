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
import re
import sqlite3
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


DEFAULT_PRICING_VERSION = "v1"
TOKEN_PRICING_USD_PER_MTOK: dict[str, dict[str, dict[str, float]]] = {
    # Costs are estimates per 1M tokens for dashboard trend comparison.
    "v1": {
        "openai": {"input": 5.0, "output": 15.0},
        "openrouter": {"input": 5.0, "output": 15.0},
        "zen": {"input": 5.0, "output": 15.0},
        "ouath": {"input": 5.0, "output": 15.0},
        "ollama": {"input": 0.0, "output": 0.0},
    }
}


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


def parse_token_usage_from_text(text: str) -> tuple[int, int]:
    prompt_matches = re.findall(r"prompt_tokens=(\d+)", text)
    completion_matches = re.findall(r"completion_tokens=(\d+)", text)
    if not prompt_matches and not completion_matches:
        return 0, 0
    prompt = int(prompt_matches[-1]) if prompt_matches else 0
    completion = int(completion_matches[-1]) if completion_matches else 0
    return prompt, completion


def infer_provider_from_model(model_name: str | None) -> str | None:
    if not model_name:
        return None
    raw = model_name.strip().lower()
    if not raw:
        return None
    if "/" in raw:
        prefix = raw.split("/", 1)[0]
        if prefix:
            return prefix
    if "gpt" in raw or "o3" in raw or "o4" in raw:
        return "openai"
    return None


def canonical_provider_name(raw: str | None) -> str:
    if not raw:
        return "unknown"
    key = raw.strip().lower()
    aliases = {
        "chatgpt": "openai",
        "ouath": "ouath",
        "openrouter": "openrouter",
        "ollama": "ollama",
        "zen": "zen",
        "openai": "openai",
    }
    return aliases.get(key, key)


def lookup_pricing(provider: str, pricing_version: str) -> tuple[float, float, str, bool]:
    version = pricing_version if pricing_version in TOKEN_PRICING_USD_PER_MTOK else DEFAULT_PRICING_VERSION
    book = TOKEN_PRICING_USD_PER_MTOK.get(version, {})
    record = book.get(provider)
    if record is None:
        return 0.0, 0.0, version, False
    return float(record.get("input", 0.0)), float(record.get("output", 0.0)), version, True


def resolve_report_path(repo_root: Path, raw: str | None) -> Path | None:
    if not raw:
        return None
    p = Path(raw)
    return p if p.is_absolute() else (repo_root / p).resolve()


def build_token_cost_rows(
    history: list[dict],
    repo_root: Path,
    pricing_version: str,
) -> tuple[list[dict], dict[str, Any]]:
    rows: list[dict] = []
    aggregate: dict[str, dict[str, float | int]] = {}
    total_cost = 0.0
    total_prompt = 0
    total_completion = 0
    total_known_provider_cost = 0.0
    unknown_provider_tasks = 0
    unknown_pricing_tasks = 0

    for entry in history:
        report_path = resolve_report_path(repo_root, str(entry.get("report_json", "")))
        if report_path is None or not report_path.exists():
            continue
        try:
            payload = json.loads(report_path.read_text())
        except json.JSONDecodeError:
            continue
        suite = str(entry.get("suite", payload.get("suite", "unknown")))
        ts = str(entry.get("timestamp_utc", payload.get("timestamp_utc", "unknown")))
        entry_cli_model = str(entry.get("cli_model") or payload.get("cli_model") or "")
        for task in payload.get("results", []):
            task_id = str(task.get("task_id", "unknown"))
            prompt_tokens = int(task.get("prompt_tokens", 0) or 0)
            completion_tokens = int(task.get("completion_tokens", 0) or 0)
            if prompt_tokens <= 0 and completion_tokens <= 0:
                text = f"{task.get('stdout', '')}\n{task.get('stderr', '')}"
                prompt_tokens, completion_tokens = parse_token_usage_from_text(text)

            provider_raw = (
                task.get("token_provider")
                or task.get("provider")
                or infer_provider_from_model(str(task.get("cli_model") or entry_cli_model))
                or "unknown"
            )
            provider = canonical_provider_name(str(provider_raw))

            in_price, out_price, used_version, known_pricing = lookup_pricing(provider, pricing_version)
            cost_usd = ((prompt_tokens * in_price) + (completion_tokens * out_price)) / 1_000_000.0

            if provider == "unknown":
                unknown_provider_tasks += 1
            if not known_pricing:
                unknown_pricing_tasks += 1
            if known_pricing:
                total_known_provider_cost += cost_usd

            total_cost += cost_usd
            total_prompt += prompt_tokens
            total_completion += completion_tokens

            agg = aggregate.setdefault(
                provider,
                {
                    "provider": provider,
                    "tasks": 0,
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "cost_usd": 0.0,
                    "known_pricing": known_pricing,
                },
            )
            agg["tasks"] = int(agg["tasks"]) + 1
            agg["prompt_tokens"] = int(agg["prompt_tokens"]) + prompt_tokens
            agg["completion_tokens"] = int(agg["completion_tokens"]) + completion_tokens
            agg["cost_usd"] = float(agg["cost_usd"]) + cost_usd
            agg["known_pricing"] = bool(agg["known_pricing"]) and known_pricing

            rows.append(
                {
                    "timestamp_utc": ts,
                    "suite": suite,
                    "task_id": task_id,
                    "provider": provider,
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                    "cost_usd": cost_usd,
                    "pricing_version": used_version,
                    "known_pricing": known_pricing,
                }
            )

    rows.sort(
        key=lambda x: (
            str(x.get("timestamp_utc", "")),
            str(x.get("suite", "")),
            str(x.get("task_id", "")),
        ),
        reverse=True,
    )
    aggregate_rows = sorted(
        aggregate.values(),
        key=lambda x: (float(x["cost_usd"]), int(x["tasks"])),
        reverse=True,
    )
    summary = {
        "pricing_version_requested": pricing_version,
        "pricing_version_used": (
            pricing_version if pricing_version in TOKEN_PRICING_USD_PER_MTOK else DEFAULT_PRICING_VERSION
        ),
        "total_cost_usd": total_cost,
        "total_cost_known_provider_usd": total_known_provider_cost,
        "total_prompt_tokens": total_prompt,
        "total_completion_tokens": total_completion,
        "total_tokens": total_prompt + total_completion,
        "unknown_provider_tasks": unknown_provider_tasks,
        "unknown_pricing_tasks": unknown_pricing_tasks,
        "providers": aggregate_rows,
    }
    return rows, summary


def is_smoke_suite(suite_name: str) -> bool:
    name = suite_name.strip().lower()
    return "smoke" in name or "mini" in name


def latest_matching(history: list[dict], predicate) -> dict | None:
    for item in reversed(history):
        if predicate(item):
            return item
    return None


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


def compute_mttf_by_lane_risk(
    conn: sqlite3.Connection,
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
) -> dict[tuple[str, str], float]:
    sql = """
        SELECT lane, risk_tier, status, COALESCE(finished_at, started_at) AS event_time
        FROM autonomous_task_outcomes
        WHERE repo = ?
          AND status IN ('failed', 'succeeded')
    """
    params: list[str] = [repo_name]
    if lane_filter:
        sql += " AND lane = ?"
        params.append(lane_filter)
    if risk_filter:
        sql += " AND risk_tier = ?"
        params.append(risk_filter)
    sql += " ORDER BY lane, risk_tier, event_time"

    rows = conn.execute(sql, tuple(params)).fetchall()

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


def query_kpis(
    conn: sqlite3.Connection,
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
) -> list[dict]:
    mttf_by_key = compute_mttf_by_lane_risk(conn, repo_name, lane_filter, risk_filter)
    sql = """
        SELECT lane, risk_tier,
               COALESCE(SUM(CASE WHEN status IN ('succeeded','failed','rolled_back') THEN 1 ELSE 0 END), 0) started,
               COALESCE(SUM(CASE WHEN status='succeeded' THEN 1 ELSE 0 END), 0) succeeded,
               COALESCE(SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END), 0) failed,
               COALESCE(SUM(verification_total), 0) ver_total,
               COALESCE(SUM(verification_passed), 0) ver_passed,
               COALESCE(SUM(rolled_back), 0) rollbacks
        FROM autonomous_task_outcomes
        WHERE repo = ?
    """
    params: list[str] = [repo_name]
    if lane_filter:
        sql += " AND lane = ?"
        params.append(lane_filter)
    if risk_filter:
        sql += " AND risk_tier = ?"
        params.append(risk_filter)
    sql += " GROUP BY lane, risk_tier ORDER BY lane, risk_tier"

    rows = conn.execute(sql, tuple(params)).fetchall()
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


def query_kpi_snapshot_trend(
    conn: sqlite3.Connection,
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
    limit: int = 20,
) -> list[dict]:
    clamped_limit = max(1, min(limit, 200))
    where = "repo = ?"
    params: list[str | int] = [repo_name]
    if lane_filter:
        where += " AND lane = ?"
        params.append(lane_filter)
    if risk_filter:
        where += " AND risk_tier = ?"
        params.append(risk_filter)
    params.append(clamped_limit)

    rows = conn.execute(
        f"""
        SELECT captured_at, lane, risk_tier,
               task_success_rate, verification_pass_rate, rollback_rate,
               tasks_started, tasks_succeeded, tasks_failed
        FROM (
            SELECT captured_at, lane, risk_tier,
                   task_success_rate, verification_pass_rate, rollback_rate,
                   tasks_started, tasks_succeeded, tasks_failed
            FROM kpi_snapshots
            WHERE {where}
            ORDER BY datetime(replace(replace(captured_at, 'T', ' '), 'Z', '')) DESC, captured_at DESC
            LIMIT ?
        ) recent
        ORDER BY datetime(replace(replace(captured_at, 'T', ' '), 'Z', '')) ASC, captured_at ASC
        """,
        tuple(params),
    ).fetchall()

    return [
        {
            "captured_at": str(r[0]),
            "lane": str(r[1]),
            "risk": str(r[2]),
            "task_success_rate": float(r[3] or 0.0),
            "verification_pass_rate": float(r[4] or 0.0),
            "rollback_rate": float(r[5] or 0.0),
            "tasks_started": int(r[6] or 0),
            "tasks_succeeded": int(r[7] or 0),
            "tasks_failed": int(r[8] or 0),
        }
        for r in rows
    ]


def query_ghost_breakdown(
    conn: sqlite3.Connection,
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
    min_samples: int = 3,
) -> list[dict]:
    threshold = max(1, min_samples)
    sql = """
        SELECT
            COALESCE(NULLIF(TRIM(ghost), ''), 'unknown') AS ghost_name,
            COALESCE(SUM(CASE WHEN status IN ('succeeded','failed','rolled_back') THEN 1 ELSE 0 END), 0) samples,
            COALESCE(SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END), 0) succeeded,
            COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) failed,
            COALESCE(SUM(CASE WHEN status = 'rolled_back' THEN 1 ELSE 0 END), 0) rolled_back
        FROM autonomous_task_outcomes
        WHERE repo = ?
    """
    params: list[str] = [repo_name]
    if lane_filter:
        sql += " AND lane = ?"
        params.append(lane_filter)
    if risk_filter:
        sql += " AND risk_tier = ?"
        params.append(risk_filter)
    sql += " GROUP BY ghost_name HAVING samples > 0 ORDER BY samples DESC, ghost_name ASC"

    rows = conn.execute(sql, tuple(params)).fetchall()
    out: list[dict] = []
    for ghost_name, samples, succeeded, failed, rolled_back in rows:
        sample_count = int(samples or 0)
        succeeded_count = int(succeeded or 0)
        out.append(
            {
                "ghost": str(ghost_name),
                "samples": sample_count,
                "succeeded": succeeded_count,
                "failed": int(failed or 0),
                "rolled_back": int(rolled_back or 0),
                "success_rate": pct(succeeded_count, sample_count),
                "sample_flag": "ok" if sample_count >= threshold else f"low-sample(<{threshold})",
                "meets_threshold": sample_count >= threshold,
            }
        )
    return out


def summarize_rate_trend(rows: list[dict], key: str) -> str:
    if not rows:
        return "n/a (no data)"
    values = [float(r.get(key, 0.0)) for r in rows]
    if len(values) == 1:
        return f"{values[0] * 100.0:.1f}% (single point)"
    first = values[0]
    last = values[-1]
    delta = last - first
    if delta > 0.001:
        direction = "up"
    elif delta < -0.001:
        direction = "down"
    else:
        direction = "flat"
    return f"{first * 100.0:.1f}% -> {last * 100.0:.1f}% ({direction}, delta={delta * 100.0:+.1f}pp)"


def render_dashboard(
    history: list[dict],
    kpis: list[dict],
    kpi_trend: list[dict],
    ghost_breakdown: list[dict],
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
    ghost_min_samples: int = 3,
    task_cost_limit: int = 20,
    token_cost_rows: list[dict] | None = None,
    token_cost_summary: dict[str, Any] | None = None,
) -> str:
    if token_cost_rows is None:
        token_cost_rows = []
    if token_cost_summary is None:
        token_cost_summary = {
            "pricing_version_requested": DEFAULT_PRICING_VERSION,
            "pricing_version_used": DEFAULT_PRICING_VERSION,
            "total_cost_usd": 0.0,
            "total_cost_known_provider_usd": 0.0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "total_tokens": 0,
            "unknown_provider_tasks": 0,
            "unknown_pricing_tasks": 0,
            "providers": [],
        }

    now = dt.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    lines: list[str] = []
    lines.append("# Athena KPI + Eval Dashboard")
    lines.append("")
    lines.append(f"- generated_utc: {now}")
    lines.append(f"- repo: `{repo_name}`")
    lines.append(f"- lane_filter: `{lane_filter or 'all'}`")
    lines.append(f"- risk_filter: `{risk_filter or 'all'}`")
    lines.append("")

    lines.append("## Latest Smoke Eval (Health)")
    smoke = latest_matching(history, lambda h: is_smoke_suite(str(h.get("suite", ""))))
    if smoke:
        lines.append(f"- suite: `{smoke.get('suite', 'unknown')}`")
        lines.append(f"- timestamp_utc: `{smoke.get('timestamp_utc', 'unknown')}`")
        lines.append(f"- gate: `{'PASS' if smoke.get('gate_ok') else 'FAIL'}`")
        lines.append(f"- overall_score: `{smoke.get('overall_score', 0):.2f}`")
        lines.append(f"- threshold: `{smoke.get('threshold', 0):.2f}`")
        lines.append(f"- task_count: `{smoke.get('task_count', 0)}`")
        lines.append(f"- exec_success_rate: `{smoke.get('exec_success_rate', 0):.2f}`")
    else:
        lines.append("- no smoke eval history found")
    lines.append("")

    lines.append("## Latest Real Eval (Quality Gate)")
    real = latest_matching(history, lambda h: not is_smoke_suite(str(h.get("suite", ""))))
    if real:
        lines.append(f"- suite: `{real.get('suite', 'unknown')}`")
        lines.append(f"- timestamp_utc: `{real.get('timestamp_utc', 'unknown')}`")
        lines.append(f"- gate: `{'PASS' if real.get('gate_ok') else 'FAIL'}`")
        lines.append(f"- overall_score: `{real.get('overall_score', 0):.2f}`")
        lines.append(f"- threshold: `{real.get('threshold', 0):.2f}`")
        lines.append(f"- task_count: `{real.get('task_count', 0)}`")
        lines.append(f"- exec_success_rate: `{real.get('exec_success_rate', 0):.2f}`")
    else:
        lines.append("- no real quality-gate eval history found")
    lines.append("")

    lines.append("## Eval Trend (All Suites)")
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

    lines.append("## KPI Trend (Snapshot History)")
    lines.append("")
    lines.append("- task_success_trend: `" + summarize_rate_trend(kpi_trend, "task_success_rate") + "`")
    lines.append("- verification_trend: `" + summarize_rate_trend(kpi_trend, "verification_pass_rate") + "`")
    lines.append("- rollback_trend: `" + summarize_rate_trend(kpi_trend, "rollback_rate") + "`")
    lines.append("")
    lines.append("| captured_at | lane | risk | task_success | verification | rollback | started |")
    lines.append("|---|---|---|---:|---:|---:|---:|")
    if kpi_trend:
        for r in kpi_trend:
            lines.append(
                f"| `{r['captured_at']}` | `{r['lane']}` | `{r['risk']}` | "
                f"{r['task_success_rate'] * 100.0:.1f}% | {r['verification_pass_rate'] * 100.0:.1f}% | "
                f"{r['rollback_rate'] * 100.0:.1f}% | {r['tasks_started']} |"
            )
    else:
        lines.append("| - | - | - | n/a | n/a | n/a | 0 |")
        lines.append("")
        lines.append("- no KPI snapshot trend found for current filters")
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

    lines.append("## Token Cost (Estimated)")
    lines.append("")
    lines.append(
        "- pricing_version: "
        f"`{token_cost_summary.get('pricing_version_used', DEFAULT_PRICING_VERSION)}`"
    )
    if str(token_cost_summary.get("pricing_version_requested")) != str(
        token_cost_summary.get("pricing_version_used")
    ):
        lines.append(
            "- pricing_version_fallback: "
            f"`{token_cost_summary.get('pricing_version_requested')}` -> "
            f"`{token_cost_summary.get('pricing_version_used')}`"
        )
    lines.append(
        "- total_tokens: "
        f"`{int(token_cost_summary.get('total_tokens', 0))}` "
        f"(prompt={int(token_cost_summary.get('total_prompt_tokens', 0))}, "
        f"completion={int(token_cost_summary.get('total_completion_tokens', 0))})"
    )
    lines.append(
        "- total_cost_usd: "
        f"`{float(token_cost_summary.get('total_cost_usd', 0.0)):.6f}` "
        f"(known_provider_cost={float(token_cost_summary.get('total_cost_known_provider_usd', 0.0)):.6f})"
    )
    lines.append(
        "- unknown_provider_tasks: "
        f"`{int(token_cost_summary.get('unknown_provider_tasks', 0))}` "
        f"unknown_pricing_tasks: `{int(token_cost_summary.get('unknown_pricing_tasks', 0))}`"
    )
    lines.append("")
    lines.append("### Aggregate by Provider")
    lines.append("")
    lines.append("| provider | tasks | prompt_tokens | completion_tokens | total_tokens | cost_usd | pricing |")
    lines.append("|---|---:|---:|---:|---:|---:|---|")
    providers = token_cost_summary.get("providers", [])
    if providers:
        for p in providers:
            prompt_tokens = int(p.get("prompt_tokens", 0))
            completion_tokens = int(p.get("completion_tokens", 0))
            lines.append(
                f"| `{p.get('provider', 'unknown')}` | {int(p.get('tasks', 0))} | "
                f"{prompt_tokens} | {completion_tokens} | {prompt_tokens + completion_tokens} | "
                f"{float(p.get('cost_usd', 0.0)):.6f} | "
                f"`{'known' if p.get('known_pricing', False) else 'unknown'}` |"
            )
    else:
        lines.append("| - | 0 | 0 | 0 | 0 | 0.000000 | n/a |")
    lines.append("")
    lines.append("### Per-Task Cost (Recent)")
    lines.append("")
    lines.append("| timestamp | suite | task | provider | prompt | completion | total | cost_usd | pricing |")
    lines.append("|---|---|---|---|---:|---:|---:|---:|---|")
    if token_cost_rows:
        limit = max(1, task_cost_limit)
        for r in token_cost_rows[:limit]:
            lines.append(
                f"| `{r['timestamp_utc']}` | `{r['suite']}` | `{r['task_id']}` | `{r['provider']}` | "
                f"{r['prompt_tokens']} | {r['completion_tokens']} | {r['total_tokens']} | "
                f"{r['cost_usd']:.6f} | `{'known' if r['known_pricing'] else 'unknown'}` |"
            )
    else:
        lines.append("| - | - | - | - | 0 | 0 | 0 | 0.000000 | n/a |")
        lines.append("")
        lines.append("- no token usage found in available eval report artifacts")
    lines.append("")

    lines.append("## Per-Ghost Performance")
    lines.append("")
    lines.append(f"- sample_threshold: `>= {max(1, ghost_min_samples)}`")
    lines.append("| ghost | samples | succeeded | failed | rolled_back | success_rate | sample_flag |")
    lines.append("|---|---:|---:|---:|---:|---:|---|")
    if ghost_breakdown:
        for row in ghost_breakdown:
            lines.append(
                f"| `{row['ghost']}` | {row['samples']} | {row['succeeded']} | {row['failed']} | "
                f"{row['rolled_back']} | {row['success_rate']} | `{row['sample_flag']}` |"
            )
        if not any(bool(r.get("meets_threshold")) for r in ghost_breakdown):
            lines.append("")
            lines.append("- sparse data: no ghosts meet the stable-sample threshold yet")
    else:
        lines.append("| - | 0 | 0 | 0 | 0 | n/a | n/a |")
    lines.append("")
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Render combined KPI + eval dashboard.")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--repo", default="athena")
    p.add_argument("--lane", default=None, help="Optional lane filter for KPI sections.")
    p.add_argument("--risk", default=None, help="Optional risk tier filter for KPI sections.")
    p.add_argument("--history-file", default="eval/results/history.jsonl")
    p.add_argument("--history-limit", type=int, default=20)
    p.add_argument("--kpi-trend-limit", type=int, default=20)
    p.add_argument(
        "--pricing-version",
        default=DEFAULT_PRICING_VERSION,
        help="Pricing mapping version for token cost estimates.",
    )
    p.add_argument(
        "--task-cost-limit",
        type=int,
        default=20,
        help="How many per-task cost rows to render.",
    )
    p.add_argument(
        "--ghost-min-samples",
        type=int,
        default=3,
        help="Minimum sample count for stable ghost-comparison labeling.",
    )
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
    kpis = query_kpis(conn, args.repo, lane_filter=args.lane, risk_filter=args.risk)
    kpi_trend = query_kpi_snapshot_trend(
        conn,
        args.repo,
        lane_filter=args.lane,
        risk_filter=args.risk,
        limit=args.kpi_trend_limit,
    )
    ghost_breakdown = query_ghost_breakdown(
        conn,
        args.repo,
        lane_filter=args.lane,
        risk_filter=args.risk,
        min_samples=args.ghost_min_samples,
    )
    conn.close()
    token_cost_rows, token_cost_summary = build_token_cost_rows(
        history,
        repo_root=repo,
        pricing_version=args.pricing_version,
    )

    content = render_dashboard(
        history,
        kpis,
        kpi_trend,
        ghost_breakdown,
        args.repo,
        lane_filter=args.lane,
        risk_filter=args.risk,
        ghost_min_samples=args.ghost_min_samples,
        task_cost_limit=args.task_cost_limit,
        token_cost_rows=token_cost_rows,
        token_cost_summary=token_cost_summary,
    )
    out_file.parent.mkdir(parents=True, exist_ok=True)
    out_file.write_text(content)
    print(f"dashboard={out_file}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
