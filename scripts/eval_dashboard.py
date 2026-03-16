#!/usr/bin/env python3
"""
Combined KPI + eval dashboard.

Reads eval history (JSONL) and autonomous KPI data from SQLite,
then renders a compact markdown dashboard.
"""

from __future__ import annotations

import argparse
import datetime as dt
import html
import json
import re
import sqlite3
from collections import defaultdict
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
    default = Path("~/.sparks/sparks.db").expanduser()
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


def table_exists(conn: sqlite3.Connection, table: str) -> bool:
    row = conn.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
        (table,),
    ).fetchone()
    return bool(row and int(row[0] or 0) > 0)


def table_columns(conn: sqlite3.Connection, table: str) -> set[str]:
    if not table_exists(conn, table):
        return set()
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    return {str(r[1]) for r in rows}


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
    if not table_exists(conn, "autonomous_task_outcomes"):
        return {}
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

    try:
        rows = conn.execute(sql, tuple(params)).fetchall()
    except sqlite3.DatabaseError:
        return {}

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
    if not table_exists(conn, "autonomous_task_outcomes"):
        return []
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

    try:
        rows = conn.execute(sql, tuple(params)).fetchall()
    except sqlite3.DatabaseError:
        return []
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
    if not table_exists(conn, "kpi_snapshots"):
        return []
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

    try:
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
    except sqlite3.DatabaseError:
        return []

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
    if not table_exists(conn, "autonomous_task_outcomes"):
        return []
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

    try:
        rows = conn.execute(sql, tuple(params)).fetchall()
    except sqlite3.DatabaseError:
        return []
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


TERMINAL_STATUSES = {"succeeded", "failed", "rolled_back"}
SUCCESS_STATUSES = {"succeeded"}
SAFETY_ERROR_KEYWORDS = ("safety", "security", "prompt injection", "policy", "guardrail")


def parse_any_ts(raw: str | None) -> dt.datetime | None:
    if not raw:
        return None
    text = str(raw).strip()
    if not text:
        return None
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M:%S.%fZ"):
        try:
            return dt.datetime.strptime(text, fmt)
        except ValueError:
            continue
    normalized = text.replace("Z", "+00:00")
    try:
        parsed = dt.datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is not None:
        return parsed.astimezone(dt.timezone.utc).replace(tzinfo=None)
    return parsed


def to_utc_text(ts: dt.datetime | None, fallback: str) -> str:
    if ts is None:
        return fallback
    return ts.strftime("%Y-%m-%dT%H:%M:%SZ")


def to_bucket_day(ts: dt.datetime | None, fallback: str = "unknown") -> str:
    if ts is None:
        return fallback
    return ts.strftime("%Y-%m-%d")


def rate(numerator: float, denominator: float) -> float | None:
    if denominator <= 0:
        return None
    return numerator / denominator


def rate_or_zero(numerator: float, denominator: float) -> float:
    value = rate(numerator, denominator)
    return float(value or 0.0)


def fmt_rate(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value * 100.0:.1f}%"


def fmt_delta_pp(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value * 100.0:+.1f}pp"


def new_adapter(name: str, source: str) -> dict[str, Any]:
    return {
        "name": name,
        "source": source,
        "rows": [],
        "source_missing": False,
        "schema_warnings": [],
    }


def adapter_status(adapters: list[dict[str, Any]]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for a in adapters:
        rows.append(
            {
                "adapter": a.get("name", "unknown"),
                "source": a.get("source", "unknown"),
                "records": len(a.get("rows", [])),
                "source_missing": bool(a.get("source_missing")),
                "schema_warnings": list(a.get("schema_warnings", [])),
            }
        )
    return rows


def adapt_eval_history(history: list[dict], source_exists: bool) -> dict[str, Any]:
    adapter = new_adapter("eval_history", "eval/results/history.jsonl")
    adapter["source_missing"] = not source_exists
    for row in history:
        ts_raw = str(row.get("timestamp_utc", "unknown"))
        ts = parse_any_ts(ts_raw)
        adapter["rows"].append(
            {
                "timestamp_utc": to_utc_text(ts, ts_raw),
                "bucket": to_bucket_day(ts),
                "suite": str(row.get("suite", "unknown")),
                "gate_ok": bool(row.get("gate_ok")),
                "overall_score": float(row.get("overall_score", 0.0) or 0.0),
                "exec_success_rate": float(row.get("exec_success_rate", 0.0) or 0.0),
                "task_count": int(row.get("task_count", 0) or 0),
            }
        )
    return adapter


def adapt_kpi_snapshots(kpi_trend: list[dict], source_exists: bool) -> dict[str, Any]:
    adapter = new_adapter("kpi_snapshots", "sqlite.kpi_snapshots")
    adapter["source_missing"] = not source_exists
    for row in kpi_trend:
        ts_raw = str(row.get("captured_at", "unknown"))
        ts = parse_any_ts(ts_raw)
        adapter["rows"].append(
            {
                "timestamp_utc": to_utc_text(ts, ts_raw),
                "bucket": to_bucket_day(ts),
                "lane": str(row.get("lane", "unknown")),
                "risk": str(row.get("risk", "unknown")),
                "task_success_rate": float(row.get("task_success_rate", 0.0) or 0.0),
                "verification_pass_rate": float(row.get("verification_pass_rate", 0.0) or 0.0),
                "rollback_rate": float(row.get("rollback_rate", 0.0) or 0.0),
                "tasks_started": int(row.get("tasks_started", 0) or 0),
            }
        )
    return adapter


def adapt_autonomous_outcomes(
    conn: sqlite3.Connection,
    repo_name: str,
    lane_filter: str | None = None,
    risk_filter: str | None = None,
    limit: int = 600,
) -> dict[str, Any]:
    adapter = new_adapter("autonomous_outcomes", "sqlite.autonomous_task_outcomes")
    cols = table_columns(conn, "autonomous_task_outcomes")
    if not cols:
        adapter["source_missing"] = True
        return adapter

    missing = [c for c in ("status", "started_at", "finished_at", "verification_total", "verification_passed", "rolled_back", "error") if c not in cols]
    if missing:
        adapter["schema_warnings"].append(f"missing columns: {', '.join(sorted(missing))}")

    where = ["1=1"]
    params: list[Any] = []
    if "repo" in cols:
        where.append("repo = ?")
        params.append(repo_name)
    else:
        adapter["schema_warnings"].append("missing column: repo (repo filter skipped)")
    if lane_filter:
        if "lane" in cols:
            where.append("lane = ?")
            params.append(lane_filter)
        else:
            adapter["schema_warnings"].append("missing column: lane (lane filter skipped)")
    if risk_filter:
        if "risk_tier" in cols:
            where.append("risk_tier = ?")
            params.append(risk_filter)
        else:
            adapter["schema_warnings"].append("missing column: risk_tier (risk filter skipped)")

    order_expr = "rowid ASC"
    if "finished_at" in cols or "started_at" in cols:
        order_expr = "datetime(replace(replace(COALESCE(finished_at, started_at, ''), 'T', ' '), 'Z', '')) ASC, rowid ASC"
    clamped_limit = max(1, min(limit, 5000))
    params.append(clamped_limit)

    def expr(col: str, fallback: str) -> str:
        return col if col in cols else f"{fallback} AS {col}"

    query = f"""
        SELECT
            {expr('task_id', "''")},
            {expr('status', "'unknown'")},
            {expr('started_at', "''")},
            {expr('finished_at', "''")},
            {expr('verification_total', "0")},
            {expr('verification_passed', "0")},
            {expr('rolled_back', "0")},
            {expr('error', "''")}
        FROM autonomous_task_outcomes
        WHERE {' AND '.join(where)}
        ORDER BY {order_expr}
        LIMIT ?
    """
    try:
        rows = conn.execute(query, tuple(params)).fetchall()
    except sqlite3.DatabaseError as exc:
        adapter["schema_warnings"].append(f"query failed: {exc}")
        return adapter

    for task_id, status, started_at, finished_at, verification_total, verification_passed, rolled_back, error in rows:
        event_time_raw = str(finished_at or started_at or "unknown")
        event_time = parse_any_ts(event_time_raw)
        status_text = str(status or "unknown")
        adapter["rows"].append(
            {
                "task_id": str(task_id or ""),
                "status": status_text,
                "terminal": status_text in TERMINAL_STATUSES,
                "succeeded": status_text in SUCCESS_STATUSES,
                "rolled_back": int(rolled_back or 0),
                "verification_total": int(verification_total or 0),
                "verification_passed": int(verification_passed or 0),
                "error": str(error or ""),
                "timestamp_utc": to_utc_text(event_time, event_time_raw),
                "bucket": to_bucket_day(event_time),
            }
        )
    return adapter


def parse_token_from_kv(content: str, key: str) -> str | None:
    match = re.search(rf"\b{re.escape(key)}=([^\s]+)", content)
    if not match:
        return None
    return match.group(1)


def adapt_route_outcomes(conn: sqlite3.Connection, limit: int = 800) -> dict[str, Any]:
    adapter = new_adapter("route_outcomes", "sqlite.memories(category=route_outcome)")
    cols = table_columns(conn, "memories")
    if not cols:
        adapter["source_missing"] = True
        return adapter
    if "category" not in cols or "content" not in cols:
        adapter["schema_warnings"].append("missing required columns in memories: category/content")
        return adapter

    where = ["category = 'route_outcome'"]
    if "active" in cols:
        where.append("active = 1")
    order_expr = "created_at ASC" if "created_at" in cols else "rowid ASC"
    created_expr = "created_at" if "created_at" in cols else "'' AS created_at"

    try:
        rows = conn.execute(
            f"""
            SELECT {created_expr}, content
            FROM memories
            WHERE {' AND '.join(where)}
            ORDER BY {order_expr}
            LIMIT ?
            """,
            (max(1, min(limit, 10000)),),
        ).fetchall()
    except sqlite3.DatabaseError as exc:
        adapter["schema_warnings"].append(f"query failed: {exc}")
        return adapter

    malformed = 0
    for created_at, content in rows:
        text = str(content or "")
        status = str(parse_token_from_kv(text, "status") or "").strip().lower()
        if status not in TERMINAL_STATUSES:
            malformed += 1
            continue
        ts_raw = str(created_at or "unknown")
        ts = parse_any_ts(ts_raw)
        adapter["rows"].append(
            {
                "route_id": str(parse_token_from_kv(text, "route_id") or ""),
                "route_type": str(parse_token_from_kv(text, "route_type") or "unknown"),
                "ghost": str(parse_token_from_kv(text, "ghost") or "unknown"),
                "status": status,
                "success": status in SUCCESS_STATUSES,
                "timestamp_utc": to_utc_text(ts, ts_raw),
                "bucket": to_bucket_day(ts),
            }
        )
    if malformed:
        adapter["schema_warnings"].append(f"ignored malformed route_outcome rows: {malformed}")
    return adapter


def adapt_self_heal(conn: sqlite3.Connection, limit: int = 1000) -> dict[str, Any]:
    adapter = new_adapter("self_heal", "sqlite.memories(category=self_heal_outcome)")
    cols = table_columns(conn, "memories")
    if not cols:
        adapter["source_missing"] = True
        return adapter
    if "category" not in cols or "content" not in cols:
        adapter["schema_warnings"].append("missing required columns in memories: category/content")
        return adapter
    where = ["category = 'self_heal_outcome'"]
    if "active" in cols:
        where.append("active = 1")
    order_expr = "created_at ASC" if "created_at" in cols else "rowid ASC"
    created_expr = "created_at" if "created_at" in cols else "'' AS created_at"
    try:
        rows = conn.execute(
            f"""
            SELECT {created_expr}, content
            FROM memories
            WHERE {' AND '.join(where)}
            ORDER BY {order_expr}
            LIMIT ?
            """,
            (max(1, min(limit, 10000)),),
        ).fetchall()
    except sqlite3.DatabaseError as exc:
        adapter["schema_warnings"].append(f"query failed: {exc}")
        return adapter

    malformed = 0
    for created_at, content in rows:
        try:
            payload = json.loads(str(content or "{}"))
        except json.JSONDecodeError:
            malformed += 1
            continue
        success = payload.get("success")
        if not isinstance(success, bool):
            malformed += 1
            continue
        ts_raw = str(created_at or "unknown")
        ts = parse_any_ts(ts_raw)
        adapter["rows"].append(
            {
                "timestamp_utc": to_utc_text(ts, ts_raw),
                "bucket": to_bucket_day(ts),
                "error_category": str(payload.get("error_category", "unknown")),
                "success": success,
            }
        )
    if malformed:
        adapter["schema_warnings"].append(f"ignored malformed self_heal_outcome rows: {malformed}")
    return adapter


def adapt_ci_monitor(conn: sqlite3.Connection, limit: int = 1000) -> dict[str, Any]:
    adapter = new_adapter("ci_monitor", "sqlite.ticket_intake_log(ci_monitor_status)")
    cols = table_columns(conn, "ticket_intake_log")
    if not cols:
        adapter["source_missing"] = True
        return adapter
    if "ci_monitor_status" not in cols:
        adapter["schema_warnings"].append("missing column: ci_monitor_status")
        return adapter
    created_expr = "created_at" if "created_at" in cols else "'' AS created_at"
    order_expr = "created_at ASC" if "created_at" in cols else "rowid ASC"
    try:
        rows = conn.execute(
            f"""
            SELECT {created_expr}, ci_monitor_status
            FROM ticket_intake_log
            WHERE ci_monitor_status IS NOT NULL
              AND TRIM(ci_monitor_status) != ''
            ORDER BY {order_expr}
            LIMIT ?
            """,
            (max(1, min(limit, 10000)),),
        ).fetchall()
    except sqlite3.DatabaseError as exc:
        adapter["schema_warnings"].append(f"query failed: {exc}")
        return adapter

    for created_at, status_raw in rows:
        status_text = str(status_raw or "").strip()
        lowered = status_text.lower()
        if any(key in lowered for key in ("pass", "success", "green", "ok")):
            status_class = "pass"
        elif any(key in lowered for key in ("fail", "error", "red", "timeout")):
            status_class = "fail"
        else:
            status_class = "unknown"
        ts_raw = str(created_at or "unknown")
        ts = parse_any_ts(ts_raw)
        adapter["rows"].append(
            {
                "timestamp_utc": to_utc_text(ts, ts_raw),
                "bucket": to_bucket_day(ts),
                "status": status_text,
                "status_class": status_class,
            }
        )
    return adapter


def adapt_memory_health(conn: sqlite3.Connection, limit: int = 4000) -> dict[str, Any]:
    adapter = new_adapter("memory_health", "sqlite.memories")
    cols = table_columns(conn, "memories")
    if not cols:
        adapter["source_missing"] = True
        return adapter
    if "category" not in cols:
        adapter["schema_warnings"].append("missing required column: category")
        return adapter

    created_expr = "created_at" if "created_at" in cols else "'' AS created_at"
    active_expr = "active" if "active" in cols else "1 AS active"
    embedding_expr = "CASE WHEN embedding IS NOT NULL THEN 1 ELSE 0 END AS has_embedding" if "embedding" in cols else "0 AS has_embedding"
    order_expr = "created_at ASC" if "created_at" in cols else "rowid ASC"

    try:
        rows = conn.execute(
            f"""
            SELECT {created_expr}, category, {active_expr}, {embedding_expr}
            FROM memories
            ORDER BY {order_expr}
            LIMIT ?
            """,
            (max(1, min(limit, 20000)),),
        ).fetchall()
    except sqlite3.DatabaseError as exc:
        adapter["schema_warnings"].append(f"query failed: {exc}")
        return adapter

    category_totals: dict[str, int] = defaultdict(int)
    per_day: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    active_total = 0
    with_embedding = 0
    for created_at, category, active, has_embedding in rows:
        cat = str(category or "unknown")
        is_active = int(active or 0) == 1
        if is_active:
            active_total += 1
        if int(has_embedding or 0) == 1:
            with_embedding += 1
        category_totals[cat] += 1
        ts_raw = str(created_at or "unknown")
        ts = parse_any_ts(ts_raw)
        bucket = to_bucket_day(ts)
        day = per_day[bucket]
        day["health_alerts"] += 1 if cat == "health_alert" else 0
        day["health_fixes"] += 1 if cat == "health_fix" else 0
        day["patterns"] += 1 if cat == "pattern" else 0
        day["route_memories"] += 1 if cat in {"route_decision", "route_outcome"} else 0
        day["self_heal_outcomes"] += 1 if cat == "self_heal_outcome" else 0

    series: list[dict[str, Any]] = []
    for bucket in sorted(per_day.keys()):
        day = per_day[bucket]
        series.append(
            {
                "bucket": bucket,
                "health_alerts": int(day.get("health_alerts", 0)),
                "health_fixes": int(day.get("health_fixes", 0)),
                "patterns": int(day.get("patterns", 0)),
                "route_memories": int(day.get("route_memories", 0)),
                "self_heal_outcomes": int(day.get("self_heal_outcomes", 0)),
            }
        )
    adapter["rows"] = series
    adapter["summary"] = {
        "active_total": active_total,
        "with_embedding": with_embedding,
        "embedding_coverage": rate(with_embedding, max(active_total, 1)),
        "category_totals": dict(sorted(category_totals.items())),
    }
    return adapter


def aggregate_binary_rate(rows: list[dict[str, Any]], rate_key: str, sample_key: str) -> float | None:
    samples = sum(int(r.get(sample_key, 0) or 0) for r in rows)
    if samples <= 0:
        return None
    weighted = sum((float(r.get(rate_key, 0.0) or 0.0) * int(r.get(sample_key, 0) or 0)) for r in rows)
    return weighted / samples


def compute_routing_quality_trend(
    route_adapter: dict[str, Any],
    outcomes_adapter: dict[str, Any],
    cohort_split: float = 0.5,
) -> dict[str, Any]:
    split = min(0.8, max(0.2, cohort_split))
    entries: list[dict[str, Any]] = []
    source_used = "memories.route_outcome"
    schema_warnings = list(route_adapter.get("schema_warnings", []))

    route_rows = list(route_adapter.get("rows", []))
    if route_rows:
        for row in route_rows:
            entries.append(
                {
                    "timestamp_utc": str(row.get("timestamp_utc", "unknown")),
                    "bucket": str(row.get("bucket", "unknown")),
                    "quality_pass": 1 if bool(row.get("success")) else 0,
                }
            )
    else:
        source_used = "autonomous_task_outcomes(proxy)"
        schema_warnings.append("route_outcome empty; using autonomous outcomes proxy")
        for row in outcomes_adapter.get("rows", []):
            status = str(row.get("status", ""))
            if status not in TERMINAL_STATUSES:
                continue
            vt = int(row.get("verification_total", 0) or 0)
            vp = int(row.get("verification_passed", 0) or 0)
            quality_pass = 1 if (status in SUCCESS_STATUSES and (vt == 0 or vp >= vt)) else 0
            entries.append(
                {
                    "timestamp_utc": str(row.get("timestamp_utc", "unknown")),
                    "bucket": str(row.get("bucket", "unknown")),
                    "quality_pass": quality_pass,
                }
            )

    if len(entries) < 2:
        return {
            "source_used": source_used,
            "source_missing": bool(route_adapter.get("source_missing")) and not outcomes_adapter.get("rows"),
            "schema_warnings": schema_warnings,
            "no_data": True,
            "series": [],
            "summary": {},
        }

    n = len(entries)
    split_idx = max(1, min(n - 1, int(n * split)))
    early = entries[:split_idx]
    late = entries[split_idx:]
    early_pass = sum(int(r["quality_pass"]) for r in early)
    late_pass = sum(int(r["quality_pass"]) for r in late)
    early_rate = rate_or_zero(early_pass, len(early))
    late_rate = rate_or_zero(late_pass, len(late))

    per_day_early: dict[str, dict[str, int]] = defaultdict(lambda: {"pass": 0, "samples": 0})
    per_day_late: dict[str, dict[str, int]] = defaultdict(lambda: {"pass": 0, "samples": 0})
    for row in early:
        bucket = str(row["bucket"])
        per_day_early[bucket]["pass"] += int(row["quality_pass"])
        per_day_early[bucket]["samples"] += 1
    for row in late:
        bucket = str(row["bucket"])
        per_day_late[bucket]["pass"] += int(row["quality_pass"])
        per_day_late[bucket]["samples"] += 1

    series: list[dict[str, Any]] = []
    for bucket in sorted(set(per_day_early) | set(per_day_late)):
        e = per_day_early.get(bucket, {"pass": 0, "samples": 0})
        l = per_day_late.get(bucket, {"pass": 0, "samples": 0})
        series.append(
            {
                "bucket": bucket,
                "early_rate": rate(e["pass"], e["samples"]),
                "late_rate": rate(l["pass"], l["samples"]),
                "early_samples": e["samples"],
                "late_samples": l["samples"],
            }
        )

    return {
        "source_used": source_used,
        "source_missing": False,
        "schema_warnings": schema_warnings,
        "no_data": False,
        "series": series,
        "summary": {
            "cohort_split": split,
            "cohort_boundary_timestamp_utc": late[0]["timestamp_utc"],
            "total_samples": n,
            "early_samples": len(early),
            "late_samples": len(late),
            "early_rate": early_rate,
            "late_rate": late_rate,
            "delta": late_rate - early_rate,
        },
    }


def compute_autonomous_completion_trend(outcomes_adapter: dict[str, Any], latest_window_days: int = 7) -> dict[str, Any]:
    rows = outcomes_adapter.get("rows", [])
    if not rows:
        return {
            "source_used": outcomes_adapter.get("source"),
            "source_missing": bool(outcomes_adapter.get("source_missing")),
            "schema_warnings": list(outcomes_adapter.get("schema_warnings", [])),
            "no_data": True,
            "series": [],
            "summary": {},
        }

    per_day: dict[str, dict[str, int]] = defaultdict(lambda: {"terminal": 0, "succeeded": 0, "verify_tasks": 0, "verify_first_pass": 0})
    for row in rows:
        bucket = str(row.get("bucket", "unknown"))
        status = str(row.get("status", ""))
        if status not in TERMINAL_STATUSES:
            continue
        day = per_day[bucket]
        day["terminal"] += 1
        if status in SUCCESS_STATUSES:
            day["succeeded"] += 1
        vt = int(row.get("verification_total", 0) or 0)
        vp = int(row.get("verification_passed", 0) or 0)
        if vt > 0:
            day["verify_tasks"] += 1
            if status in SUCCESS_STATUSES and vp >= vt:
                day["verify_first_pass"] += 1

    series: list[dict[str, Any]] = []
    for bucket in sorted(per_day.keys()):
        d = per_day[bucket]
        series.append(
            {
                "bucket": bucket,
                "terminal_tasks": d["terminal"],
                "completion_rate": rate(d["succeeded"], d["terminal"]),
                "verify_tasks": d["verify_tasks"],
                "first_pass_verify_rate": rate(d["verify_first_pass"], d["verify_tasks"]),
            }
        )

    if not series:
        return {
            "source_used": outcomes_adapter.get("source"),
            "source_missing": bool(outcomes_adapter.get("source_missing")),
            "schema_warnings": list(outcomes_adapter.get("schema_warnings", [])),
            "no_data": True,
            "series": [],
            "summary": {},
        }

    window = series[-max(1, latest_window_days) :]
    completion_latest = aggregate_binary_rate(window, "completion_rate", "terminal_tasks")
    first_pass_latest = aggregate_binary_rate(window, "first_pass_verify_rate", "verify_tasks")
    return {
        "source_used": outcomes_adapter.get("source"),
        "source_missing": False,
        "schema_warnings": list(outcomes_adapter.get("schema_warnings", [])),
        "no_data": False,
        "series": series,
        "summary": {
            "latest_window_days": len(window),
            "latest_completion_rate": completion_latest,
            "latest_first_pass_verify_rate": first_pass_latest,
            "terminal_tasks": sum(int(r.get("terminal_tasks", 0) or 0) for r in series),
            "verify_tasks": sum(int(r.get("verify_tasks", 0) or 0) for r in series),
        },
    }


def compute_ci_self_heal_rollbacks(
    self_heal_adapter: dict[str, Any],
    outcomes_adapter: dict[str, Any],
    ci_adapter: dict[str, Any],
) -> dict[str, Any]:
    per_day: dict[str, dict[str, int]] = defaultdict(
        lambda: {
            "self_heal_attempts": 0,
            "self_heal_successes": 0,
            "rollbacks": 0,
            "terminal_tasks": 0,
            "ci_pass": 0,
            "ci_fail": 0,
        }
    )

    for row in self_heal_adapter.get("rows", []):
        day = per_day[str(row.get("bucket", "unknown"))]
        day["self_heal_attempts"] += 1
        day["self_heal_successes"] += 1 if bool(row.get("success")) else 0

    for row in outcomes_adapter.get("rows", []):
        status = str(row.get("status", ""))
        if status not in TERMINAL_STATUSES:
            continue
        day = per_day[str(row.get("bucket", "unknown"))]
        day["terminal_tasks"] += 1
        day["rollbacks"] += int(row.get("rolled_back", 0) or 0)

    latest_ci_status = None
    latest_ci_ts = None
    for row in ci_adapter.get("rows", []):
        bucket = str(row.get("bucket", "unknown"))
        day = per_day[bucket]
        status_class = str(row.get("status_class", "unknown"))
        if status_class == "pass":
            day["ci_pass"] += 1
        elif status_class == "fail":
            day["ci_fail"] += 1
        latest_ci_status = str(row.get("status", "unknown"))
        latest_ci_ts = str(row.get("timestamp_utc", "unknown"))

    series: list[dict[str, Any]] = []
    for bucket in sorted(per_day.keys()):
        d = per_day[bucket]
        series.append(
            {
                "bucket": bucket,
                "self_heal_attempts": d["self_heal_attempts"],
                "self_heal_successes": d["self_heal_successes"],
                "self_heal_success_rate": rate(d["self_heal_successes"], d["self_heal_attempts"]),
                "rollbacks": d["rollbacks"],
                "terminal_tasks": d["terminal_tasks"],
                "rollback_rate": rate(d["rollbacks"], d["terminal_tasks"]),
                "ci_pass": d["ci_pass"],
                "ci_fail": d["ci_fail"],
            }
        )

    schema_warnings = (
        list(self_heal_adapter.get("schema_warnings", []))
        + list(outcomes_adapter.get("schema_warnings", []))
        + list(ci_adapter.get("schema_warnings", []))
    )
    source_missing = (
        bool(self_heal_adapter.get("source_missing"))
        and bool(outcomes_adapter.get("source_missing"))
        and bool(ci_adapter.get("source_missing"))
    )
    return {
        "source_used": "memories.self_heal_outcome + autonomous_task_outcomes + ticket_intake_log.ci_monitor_status",
        "source_missing": source_missing,
        "schema_warnings": schema_warnings,
        "no_data": not bool(series),
        "series": series,
        "summary": {
            "self_heal_attempts": sum(int(r.get("self_heal_attempts", 0) or 0) for r in series),
            "self_heal_successes": sum(int(r.get("self_heal_successes", 0) or 0) for r in series),
            "rollbacks": sum(int(r.get("rollbacks", 0) or 0) for r in series),
            "ci_pass": sum(int(r.get("ci_pass", 0) or 0) for r in series),
            "ci_fail": sum(int(r.get("ci_fail", 0) or 0) for r in series),
            "latest_ci_status": latest_ci_status,
            "latest_ci_timestamp_utc": latest_ci_ts,
        },
    }


def compute_safety_events(outcomes_adapter: dict[str, Any], memory_adapter: dict[str, Any]) -> dict[str, Any]:
    per_day: dict[str, dict[str, int]] = defaultdict(
        lambda: {"failed_tasks": 0, "rolled_back_tasks": 0, "safety_keyword_errors": 0, "health_alerts": 0}
    )
    for row in outcomes_adapter.get("rows", []):
        bucket = str(row.get("bucket", "unknown"))
        status = str(row.get("status", ""))
        if status == "failed":
            per_day[bucket]["failed_tasks"] += 1
        per_day[bucket]["rolled_back_tasks"] += int(row.get("rolled_back", 0) or 0)
        error_text = str(row.get("error", "")).lower()
        if error_text and any(key in error_text for key in SAFETY_ERROR_KEYWORDS):
            per_day[bucket]["safety_keyword_errors"] += 1

    for row in memory_adapter.get("rows", []):
        bucket = str(row.get("bucket", "unknown"))
        per_day[bucket]["health_alerts"] += int(row.get("health_alerts", 0) or 0)

    series: list[dict[str, Any]] = []
    for bucket in sorted(per_day.keys()):
        d = per_day[bucket]
        series.append(
            {
                "bucket": bucket,
                "failed_tasks": d["failed_tasks"],
                "rolled_back_tasks": d["rolled_back_tasks"],
                "safety_keyword_errors": d["safety_keyword_errors"],
                "health_alerts": d["health_alerts"],
            }
        )

    return {
        "source_used": "autonomous_task_outcomes + memories(health_alert)",
        "source_missing": bool(outcomes_adapter.get("source_missing")) and bool(memory_adapter.get("source_missing")),
        "schema_warnings": list(outcomes_adapter.get("schema_warnings", [])) + list(memory_adapter.get("schema_warnings", [])),
        "no_data": not bool(series),
        "series": series,
        "summary": {
            "failed_tasks": sum(int(r.get("failed_tasks", 0) or 0) for r in series),
            "rolled_back_tasks": sum(int(r.get("rolled_back_tasks", 0) or 0) for r in series),
            "safety_keyword_errors": sum(int(r.get("safety_keyword_errors", 0) or 0) for r in series),
            "health_alerts": sum(int(r.get("health_alerts", 0) or 0) for r in series),
        },
    }


def compute_memory_health_signals(memory_adapter: dict[str, Any]) -> dict[str, Any]:
    summary = dict(memory_adapter.get("summary", {}))
    source_missing = bool(memory_adapter.get("source_missing"))
    rows = list(memory_adapter.get("rows", []))
    if not rows:
        return {
            "source_used": memory_adapter.get("source"),
            "source_missing": source_missing,
            "schema_warnings": list(memory_adapter.get("schema_warnings", [])),
            "no_data": True,
            "series": [],
            "summary": summary,
        }
    fixes = sum(int(r.get("health_fixes", 0) or 0) for r in rows)
    alerts = sum(int(r.get("health_alerts", 0) or 0) for r in rows)
    summary["fix_to_alert_ratio"] = rate(fixes, alerts)
    return {
        "source_used": memory_adapter.get("source"),
        "source_missing": source_missing,
        "schema_warnings": list(memory_adapter.get("schema_warnings", [])),
        "no_data": False,
        "series": rows,
        "summary": summary,
    }


def build_data_lineage_map(cohort_split: float) -> list[dict[str, str]]:
    return [
        {
            "metric": "Routing quality (early vs late)",
            "source": "memories(category=route_outcome); fallback autonomous_task_outcomes",
            "transform": f"status -> success flag, split chronologically at {cohort_split:.2f}, aggregate daily success rate",
            "chart": "Routing Quality Trend (Early vs Late Cohorts)",
        },
        {
            "metric": "Autonomous completion rate",
            "source": "autonomous_task_outcomes(status, finished_at, started_at)",
            "transform": "daily succeeded / terminal tasks",
            "chart": "Autonomous Completion & First-pass Verify",
        },
        {
            "metric": "First-pass verify rate",
            "source": "autonomous_task_outcomes(verification_total, verification_passed, status)",
            "transform": "daily succeeded with verification_passed>=verification_total / tasks with verification_total>0",
            "chart": "Autonomous Completion & First-pass Verify",
        },
        {
            "metric": "CI self-heal outcomes",
            "source": "memories(category=self_heal_outcome)",
            "transform": "JSON success flag aggregated into daily attempts and success rate",
            "chart": "CI Self-heal / Rollback Outcomes",
        },
        {
            "metric": "Rollback outcomes",
            "source": "autonomous_task_outcomes(rolled_back, status)",
            "transform": "daily rollback counts and rollback_rate=rollbacks/terminal",
            "chart": "CI Self-heal / Rollback Outcomes",
        },
        {
            "metric": "Safety events",
            "source": "autonomous_task_outcomes(error,status,rolled_back) + memories(category=health_alert)",
            "transform": "daily failed/rollback/error-keyword counters plus health alerts",
            "chart": "Safety Events",
        },
        {
            "metric": "Memory health signals",
            "source": "memories(category,active,embedding,created_at)",
            "transform": "category activity trends + embedding coverage + fix/alert ratio",
            "chart": "Memory Health Signals",
        },
    ]


def build_observability_bundle(
    conn: sqlite3.Connection,
    history: list[dict],
    history_source_exists: bool,
    kpi_trend: list[dict],
    repo_name: str,
    lane_filter: str | None,
    risk_filter: str | None,
    routing_cohort_split: float,
) -> dict[str, Any]:
    eval_adapter = adapt_eval_history(history, source_exists=history_source_exists)
    kpi_adapter = adapt_kpi_snapshots(kpi_trend, source_exists=table_exists(conn, "kpi_snapshots"))
    outcomes_adapter = adapt_autonomous_outcomes(
        conn,
        repo_name=repo_name,
        lane_filter=lane_filter,
        risk_filter=risk_filter,
    )
    route_adapter = adapt_route_outcomes(conn)
    self_heal_adapter = adapt_self_heal(conn)
    ci_adapter = adapt_ci_monitor(conn)
    memory_adapter = adapt_memory_health(conn)

    routing_quality = compute_routing_quality_trend(
        route_adapter=route_adapter,
        outcomes_adapter=outcomes_adapter,
        cohort_split=routing_cohort_split,
    )
    autonomous_completion = compute_autonomous_completion_trend(outcomes_adapter)
    ci_self_heal = compute_ci_self_heal_rollbacks(self_heal_adapter, outcomes_adapter, ci_adapter)
    safety_events = compute_safety_events(outcomes_adapter, memory_adapter)
    memory_health = compute_memory_health_signals(memory_adapter)
    adapters = [
        eval_adapter,
        kpi_adapter,
        outcomes_adapter,
        route_adapter,
        self_heal_adapter,
        ci_adapter,
        memory_adapter,
    ]

    return {
        "adapters": adapters,
        "adapter_status": adapter_status(adapters),
        "routing_quality": routing_quality,
        "autonomous_completion": autonomous_completion,
        "ci_self_heal": ci_self_heal,
        "safety_events": safety_events,
        "memory_health": memory_health,
        "data_lineage": build_data_lineage_map(routing_cohort_split),
    }


def render_adapter_status_markdown(lines: list[str], observability: dict[str, Any]) -> None:
    lines.append("## Source Adapter Status")
    lines.append("")
    lines.append("| adapter | source | records | source_missing | schema_warnings |")
    lines.append("|---|---|---:|---|---|")
    rows = observability.get("adapter_status", [])
    if not rows:
        lines.append("| - | - | 0 | true | no adapter status available |")
    else:
        for row in rows:
            warnings = "; ".join(str(w) for w in row.get("schema_warnings", [])) or "none"
            lines.append(
                f"| `{row.get('adapter', 'unknown')}` | `{row.get('source', 'unknown')}` | "
                f"{int(row.get('records', 0) or 0)} | "
                f"`{str(bool(row.get('source_missing'))).lower()}` | {warnings} |"
            )
    lines.append("")


def render_observability_sections_markdown(lines: list[str], observability: dict[str, Any]) -> None:
    routing = observability.get("routing_quality", {})
    lines.append("## Routing Quality Trend (Early vs Late Cohorts)")
    lines.append("")
    if routing.get("no_data"):
        lines.append("- no routing quality data available")
    else:
        summary = routing.get("summary", {})
        lines.append(f"- source_used: `{routing.get('source_used', 'unknown')}`")
        lines.append(f"- early_cohort_rate: `{fmt_rate(summary.get('early_rate'))}`")
        lines.append(f"- late_cohort_rate: `{fmt_rate(summary.get('late_rate'))}`")
        lines.append(f"- delta_late_minus_early: `{fmt_delta_pp(summary.get('delta'))}`")
        lines.append(f"- cohort_boundary_timestamp_utc: `{summary.get('cohort_boundary_timestamp_utc', 'unknown')}`")
    warnings = routing.get("schema_warnings", [])
    if warnings:
        lines.append(f"- schema_warnings: `{' | '.join(str(w) for w in warnings)}`")
    lines.append("")
    lines.append("| day | early_rate | late_rate | early_samples | late_samples |")
    lines.append("|---|---:|---:|---:|---:|")
    if routing.get("series"):
        for row in routing["series"]:
            lines.append(
                f"| `{row.get('bucket', 'unknown')}` | {fmt_rate(row.get('early_rate'))} | "
                f"{fmt_rate(row.get('late_rate'))} | {int(row.get('early_samples', 0) or 0)} | "
                f"{int(row.get('late_samples', 0) or 0)} |"
            )
    else:
        lines.append("| - | n/a | n/a | 0 | 0 |")
    lines.append("")

    auto = observability.get("autonomous_completion", {})
    lines.append("## Autonomous Completion & First-pass Verify")
    lines.append("")
    if auto.get("no_data"):
        lines.append("- no autonomous completion trend data available")
    else:
        summary = auto.get("summary", {})
        lines.append(f"- latest_completion_rate: `{fmt_rate(summary.get('latest_completion_rate'))}`")
        lines.append(f"- latest_first_pass_verify_rate: `{fmt_rate(summary.get('latest_first_pass_verify_rate'))}`")
        lines.append(f"- latest_window_days: `{int(summary.get('latest_window_days', 0) or 0)}`")
    warnings = auto.get("schema_warnings", [])
    if warnings:
        lines.append(f"- schema_warnings: `{' | '.join(str(w) for w in warnings)}`")
    lines.append("")
    lines.append("| day | completion_rate | first_pass_verify_rate | terminal_tasks | verify_tasks |")
    lines.append("|---|---:|---:|---:|---:|")
    if auto.get("series"):
        for row in auto["series"]:
            lines.append(
                f"| `{row.get('bucket', 'unknown')}` | {fmt_rate(row.get('completion_rate'))} | "
                f"{fmt_rate(row.get('first_pass_verify_rate'))} | {int(row.get('terminal_tasks', 0) or 0)} | "
                f"{int(row.get('verify_tasks', 0) or 0)} |"
            )
    else:
        lines.append("| - | n/a | n/a | 0 | 0 |")
    lines.append("")

    ci = observability.get("ci_self_heal", {})
    lines.append("## CI Self-heal / Rollback Outcomes")
    lines.append("")
    if ci.get("no_data"):
        lines.append("- no CI/self-heal/rollback data available")
    else:
        summary = ci.get("summary", {})
        lines.append(f"- self_heal_attempts: `{int(summary.get('self_heal_attempts', 0) or 0)}`")
        lines.append(f"- self_heal_successes: `{int(summary.get('self_heal_successes', 0) or 0)}`")
        lines.append(f"- rollbacks: `{int(summary.get('rollbacks', 0) or 0)}`")
        lines.append(f"- ci_pass: `{int(summary.get('ci_pass', 0) or 0)}` ci_fail: `{int(summary.get('ci_fail', 0) or 0)}`")
        if summary.get("latest_ci_status"):
            lines.append(
                f"- latest_ci_status: `{summary.get('latest_ci_status')}` @ `{summary.get('latest_ci_timestamp_utc', 'unknown')}`"
            )
    warnings = ci.get("schema_warnings", [])
    if warnings:
        lines.append(f"- schema_warnings: `{' | '.join(str(w) for w in warnings)}`")
    lines.append("")
    lines.append("| day | self_heal_attempts | self_heal_success_rate | rollbacks | rollback_rate | ci_pass | ci_fail |")
    lines.append("|---|---:|---:|---:|---:|---:|---:|")
    if ci.get("series"):
        for row in ci["series"]:
            lines.append(
                f"| `{row.get('bucket', 'unknown')}` | {int(row.get('self_heal_attempts', 0) or 0)} | "
                f"{fmt_rate(row.get('self_heal_success_rate'))} | {int(row.get('rollbacks', 0) or 0)} | "
                f"{fmt_rate(row.get('rollback_rate'))} | {int(row.get('ci_pass', 0) or 0)} | {int(row.get('ci_fail', 0) or 0)} |"
            )
    else:
        lines.append("| - | 0 | n/a | 0 | n/a | 0 | 0 |")
    lines.append("")

    safety = observability.get("safety_events", {})
    lines.append("## Safety Events")
    lines.append("")
    if safety.get("no_data"):
        lines.append("- no safety event data available")
    else:
        summary = safety.get("summary", {})
        lines.append(f"- failed_tasks: `{int(summary.get('failed_tasks', 0) or 0)}`")
        lines.append(f"- rolled_back_tasks: `{int(summary.get('rolled_back_tasks', 0) or 0)}`")
        lines.append(f"- safety_keyword_errors: `{int(summary.get('safety_keyword_errors', 0) or 0)}`")
        lines.append(f"- health_alerts: `{int(summary.get('health_alerts', 0) or 0)}`")
    lines.append("")
    lines.append("| day | failed_tasks | rolled_back_tasks | safety_keyword_errors | health_alerts |")
    lines.append("|---|---:|---:|---:|---:|")
    if safety.get("series"):
        for row in safety["series"]:
            lines.append(
                f"| `{row.get('bucket', 'unknown')}` | {int(row.get('failed_tasks', 0) or 0)} | "
                f"{int(row.get('rolled_back_tasks', 0) or 0)} | {int(row.get('safety_keyword_errors', 0) or 0)} | "
                f"{int(row.get('health_alerts', 0) or 0)} |"
            )
    else:
        lines.append("| - | 0 | 0 | 0 | 0 |")
    lines.append("")

    memory = observability.get("memory_health", {})
    lines.append("## Memory Health Signals")
    lines.append("")
    if memory.get("no_data"):
        lines.append("- no memory health data available")
    else:
        summary = memory.get("summary", {})
        lines.append(f"- active_memories: `{int(summary.get('active_total', 0) or 0)}`")
        lines.append(f"- embedding_coverage: `{fmt_rate(summary.get('embedding_coverage'))}`")
        lines.append(f"- fix_to_alert_ratio: `{fmt_rate(summary.get('fix_to_alert_ratio'))}`")
    warnings = memory.get("schema_warnings", [])
    if warnings:
        lines.append(f"- schema_warnings: `{' | '.join(str(w) for w in warnings)}`")
    lines.append("")
    lines.append("| day | health_alerts | health_fixes | patterns | route_memories | self_heal_outcomes |")
    lines.append("|---|---:|---:|---:|---:|---:|")
    if memory.get("series"):
        for row in memory["series"]:
            lines.append(
                f"| `{row.get('bucket', 'unknown')}` | {int(row.get('health_alerts', 0) or 0)} | "
                f"{int(row.get('health_fixes', 0) or 0)} | {int(row.get('patterns', 0) or 0)} | "
                f"{int(row.get('route_memories', 0) or 0)} | {int(row.get('self_heal_outcomes', 0) or 0)} |"
            )
    else:
        lines.append("| - | 0 | 0 | 0 | 0 | 0 |")
    lines.append("")

    lines.append("## Data Lineage")
    lines.append("")
    lines.append("| metric | source | transform | chart |")
    lines.append("|---|---|---|---|")
    lineage = observability.get("data_lineage", [])
    if not lineage:
        lines.append("| - | - | - | - |")
    else:
        for row in lineage:
            lines.append(
                f"| {row.get('metric', '-')} | `{row.get('source', '-')}` | {row.get('transform', '-')} | {row.get('chart', '-')} |"
            )
    lines.append("")


def _svg_polyline_points(values: list[float | None], width: int, height: int, padding: int) -> str:
    usable_w = max(1, width - (2 * padding))
    usable_h = max(1, height - (2 * padding))
    filtered = [v for v in values if v is not None]
    if not filtered:
        return ""
    points: list[str] = []
    for idx, value in enumerate(values):
        if value is None:
            continue
        x = padding + (usable_w * idx / max(1, len(values) - 1))
        clamped = max(0.0, min(1.0, float(value)))
        y = padding + ((1.0 - clamped) * usable_h)
        points.append(f"{x:.2f},{y:.2f}")
    return " ".join(points)


def render_rate_chart_html(series: list[dict[str, Any]], key_specs: list[tuple[str, str, str]], title: str) -> str:
    if not series:
        return "<p>no data</p>"
    width = 720
    height = 220
    padding = 18
    x_labels = [str(row.get("bucket", "?")) for row in series]
    parts = [
        f"<figure><figcaption><strong>{html.escape(title)}</strong></figcaption>",
        f"<svg width='{width}' height='{height}' viewBox='0 0 {width} {height}' role='img' aria-label='{html.escape(title)}'>",
        f"<rect x='0' y='0' width='{width}' height='{height}' fill='white' stroke='#d0d7de'/>",
        f"<line x1='{padding}' y1='{height - padding}' x2='{width - padding}' y2='{height - padding}' stroke='#8c959f' stroke-width='1'/>",
        f"<line x1='{padding}' y1='{padding}' x2='{padding}' y2='{height - padding}' stroke='#8c959f' stroke-width='1'/>",
    ]
    for key, label, color in key_specs:
        values = [row.get(key) if isinstance(row.get(key), (int, float)) else None for row in series]
        points = _svg_polyline_points(values, width, height, padding)
        if points:
            parts.append(
                f"<polyline fill='none' stroke='{html.escape(color)}' stroke-width='2' points='{points}' />"
            )
            if len(points.split()) == 1:
                x, y = points.split()[0].split(",")
                parts.append(f"<circle cx='{x}' cy='{y}' r='3' fill='{html.escape(color)}' />")
        parts.append(
            f"<text x='{padding + 8}' y='{padding + 14 + (16 * key_specs.index((key, label, color)))}' fill='{html.escape(color)}'>{html.escape(label)}</text>"
        )
    if x_labels:
        parts.append(
            f"<text x='{padding}' y='{height - 4}' fill='#6e7781' font-size='11'>{html.escape(x_labels[0])}</text>"
        )
        parts.append(
            f"<text x='{width - padding - 90}' y='{height - 4}' fill='#6e7781' font-size='11'>{html.escape(x_labels[-1])}</text>"
        )
    parts.append("</svg></figure>")
    return "".join(parts)


def render_observability_sections_html(parts: list[str], observability: dict[str, Any]) -> None:
    parts.append("<h2>Source Adapter Status</h2>")
    status_rows: list[list[str]] = []
    for row in observability.get("adapter_status", []):
        status_rows.append(
            [
                str(row.get("adapter", "unknown")),
                str(row.get("source", "unknown")),
                str(int(row.get("records", 0) or 0)),
                "true" if bool(row.get("source_missing")) else "false",
                "; ".join(str(w) for w in row.get("schema_warnings", [])) or "none",
            ]
        )
    if not status_rows:
        status_rows.append(["-", "-", "0", "true", "no adapter status available"])
    parts.append(
        _render_html_table(
            ["adapter", "source", "records", "source_missing", "schema_warnings"],
            status_rows,
        )
    )

    routing = observability.get("routing_quality", {})
    parts.append("<h2>Routing Quality Trend (Early vs Late Cohorts)</h2>")
    if routing.get("no_data"):
        parts.append("<p>no routing quality data available</p>")
    else:
        summary = routing.get("summary", {})
        parts.append("<ul>")
        parts.append(f"<li>source_used: <code>{html.escape(str(routing.get('source_used', 'unknown')))}</code></li>")
        parts.append(f"<li>early_cohort_rate: <code>{html.escape(fmt_rate(summary.get('early_rate')))}</code></li>")
        parts.append(f"<li>late_cohort_rate: <code>{html.escape(fmt_rate(summary.get('late_rate')))}</code></li>")
        parts.append(f"<li>delta_late_minus_early: <code>{html.escape(fmt_delta_pp(summary.get('delta')))}</code></li>")
        parts.append("</ul>")
    if routing.get("schema_warnings"):
        parts.append(
            f"<p>schema_warnings: <code>{html.escape(' | '.join(str(w) for w in routing['schema_warnings']))}</code></p>"
        )
    parts.append(
        render_rate_chart_html(
            routing.get("series", []),
            [("early_rate", "Early Cohort", "#1f77b4"), ("late_rate", "Late Cohort", "#d62728")],
            "Routing quality by day",
        )
    )
    routing_rows: list[list[str]] = []
    for row in routing.get("series", []):
        routing_rows.append(
            [
                str(row.get("bucket", "unknown")),
                fmt_rate(row.get("early_rate")),
                fmt_rate(row.get("late_rate")),
                str(int(row.get("early_samples", 0) or 0)),
                str(int(row.get("late_samples", 0) or 0)),
            ]
        )
    if not routing_rows:
        routing_rows.append(["-", "n/a", "n/a", "0", "0"])
    parts.append(_render_html_table(["day", "early_rate", "late_rate", "early_samples", "late_samples"], routing_rows))

    auto = observability.get("autonomous_completion", {})
    parts.append("<h2>Autonomous Completion &amp; First-pass Verify</h2>")
    if auto.get("no_data"):
        parts.append("<p>no autonomous completion trend data available</p>")
    else:
        summary = auto.get("summary", {})
        parts.append("<ul>")
        parts.append(f"<li>latest_completion_rate: <code>{html.escape(fmt_rate(summary.get('latest_completion_rate')))}</code></li>")
        parts.append(
            f"<li>latest_first_pass_verify_rate: <code>{html.escape(fmt_rate(summary.get('latest_first_pass_verify_rate')))}</code></li>"
        )
        parts.append("</ul>")
    parts.append(
        render_rate_chart_html(
            auto.get("series", []),
            [("completion_rate", "Completion", "#2ca02c"), ("first_pass_verify_rate", "First-pass Verify", "#ff7f0e")],
            "Autonomous completion and first-pass verify rates",
        )
    )
    auto_rows: list[list[str]] = []
    for row in auto.get("series", []):
        auto_rows.append(
            [
                str(row.get("bucket", "unknown")),
                fmt_rate(row.get("completion_rate")),
                fmt_rate(row.get("first_pass_verify_rate")),
                str(int(row.get("terminal_tasks", 0) or 0)),
                str(int(row.get("verify_tasks", 0) or 0)),
            ]
        )
    if not auto_rows:
        auto_rows.append(["-", "n/a", "n/a", "0", "0"])
    parts.append(
        _render_html_table(
            ["day", "completion_rate", "first_pass_verify_rate", "terminal_tasks", "verify_tasks"],
            auto_rows,
        )
    )

    ci = observability.get("ci_self_heal", {})
    parts.append("<h2>CI Self-heal / Rollback Outcomes</h2>")
    if ci.get("no_data"):
        parts.append("<p>no CI/self-heal/rollback data available</p>")
    else:
        summary = ci.get("summary", {})
        parts.append("<ul>")
        parts.append(f"<li>self_heal_attempts: <code>{int(summary.get('self_heal_attempts', 0) or 0)}</code></li>")
        parts.append(f"<li>self_heal_successes: <code>{int(summary.get('self_heal_successes', 0) or 0)}</code></li>")
        parts.append(f"<li>rollbacks: <code>{int(summary.get('rollbacks', 0) or 0)}</code></li>")
        parts.append("</ul>")
    parts.append(
        render_rate_chart_html(
            ci.get("series", []),
            [("self_heal_success_rate", "Self-heal Success Rate", "#9467bd"), ("rollback_rate", "Rollback Rate", "#8c564b")],
            "CI self-heal and rollback rates",
        )
    )
    ci_rows: list[list[str]] = []
    for row in ci.get("series", []):
        ci_rows.append(
            [
                str(row.get("bucket", "unknown")),
                str(int(row.get("self_heal_attempts", 0) or 0)),
                fmt_rate(row.get("self_heal_success_rate")),
                str(int(row.get("rollbacks", 0) or 0)),
                fmt_rate(row.get("rollback_rate")),
                str(int(row.get("ci_pass", 0) or 0)),
                str(int(row.get("ci_fail", 0) or 0)),
            ]
        )
    if not ci_rows:
        ci_rows.append(["-", "0", "n/a", "0", "n/a", "0", "0"])
    parts.append(
        _render_html_table(
            ["day", "self_heal_attempts", "self_heal_success_rate", "rollbacks", "rollback_rate", "ci_pass", "ci_fail"],
            ci_rows,
        )
    )

    safety = observability.get("safety_events", {})
    parts.append("<h2>Safety Events</h2>")
    if safety.get("no_data"):
        parts.append("<p>no safety event data available</p>")
    else:
        summary = safety.get("summary", {})
        parts.append("<ul>")
        parts.append(f"<li>failed_tasks: <code>{int(summary.get('failed_tasks', 0) or 0)}</code></li>")
        parts.append(f"<li>rolled_back_tasks: <code>{int(summary.get('rolled_back_tasks', 0) or 0)}</code></li>")
        parts.append(f"<li>safety_keyword_errors: <code>{int(summary.get('safety_keyword_errors', 0) or 0)}</code></li>")
        parts.append(f"<li>health_alerts: <code>{int(summary.get('health_alerts', 0) or 0)}</code></li>")
        parts.append("</ul>")
    safety_rows: list[list[str]] = []
    for row in safety.get("series", []):
        safety_rows.append(
            [
                str(row.get("bucket", "unknown")),
                str(int(row.get("failed_tasks", 0) or 0)),
                str(int(row.get("rolled_back_tasks", 0) or 0)),
                str(int(row.get("safety_keyword_errors", 0) or 0)),
                str(int(row.get("health_alerts", 0) or 0)),
            ]
        )
    if not safety_rows:
        safety_rows.append(["-", "0", "0", "0", "0"])
    parts.append(_render_html_table(["day", "failed_tasks", "rolled_back_tasks", "safety_keyword_errors", "health_alerts"], safety_rows))

    memory = observability.get("memory_health", {})
    parts.append("<h2>Memory Health Signals</h2>")
    if memory.get("no_data"):
        parts.append("<p>no memory health data available</p>")
    else:
        summary = memory.get("summary", {})
        parts.append("<ul>")
        parts.append(f"<li>active_memories: <code>{int(summary.get('active_total', 0) or 0)}</code></li>")
        parts.append(f"<li>embedding_coverage: <code>{html.escape(fmt_rate(summary.get('embedding_coverage')))}</code></li>")
        parts.append(f"<li>fix_to_alert_ratio: <code>{html.escape(fmt_rate(summary.get('fix_to_alert_ratio')))}</code></li>")
        parts.append("</ul>")
    memory_rows: list[list[str]] = []
    for row in memory.get("series", []):
        memory_rows.append(
            [
                str(row.get("bucket", "unknown")),
                str(int(row.get("health_alerts", 0) or 0)),
                str(int(row.get("health_fixes", 0) or 0)),
                str(int(row.get("patterns", 0) or 0)),
                str(int(row.get("route_memories", 0) or 0)),
                str(int(row.get("self_heal_outcomes", 0) or 0)),
            ]
        )
    if not memory_rows:
        memory_rows.append(["-", "0", "0", "0", "0", "0"])
    parts.append(
        _render_html_table(
            ["day", "health_alerts", "health_fixes", "patterns", "route_memories", "self_heal_outcomes"],
            memory_rows,
        )
    )

    parts.append("<h2>Data Lineage</h2>")
    lineage_rows: list[list[str]] = []
    for row in observability.get("data_lineage", []):
        lineage_rows.append(
            [
                str(row.get("metric", "-")),
                str(row.get("source", "-")),
                str(row.get("transform", "-")),
                str(row.get("chart", "-")),
            ]
        )
    if not lineage_rows:
        lineage_rows.append(["-", "-", "-", "-"])
    parts.append(_render_html_table(["metric", "source", "transform", "chart"], lineage_rows))


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
    observability: dict[str, Any] | None = None,
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
    if observability is None:
        observability = {
            "adapter_status": [],
            "routing_quality": {"no_data": True, "series": [], "summary": {}},
            "autonomous_completion": {"no_data": True, "series": [], "summary": {}},
            "ci_self_heal": {"no_data": True, "series": [], "summary": {}},
            "safety_events": {"no_data": True, "series": [], "summary": {}},
            "memory_health": {"no_data": True, "series": [], "summary": {}},
            "data_lineage": [],
        }

    now = dt.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    lines: list[str] = []
    lines.append("# Sparks KPI + Eval Dashboard")
    lines.append("")
    lines.append(f"- generated_utc: {now}")
    lines.append(f"- repo: `{repo_name}`")
    lines.append(f"- lane_filter: `{lane_filter or 'all'}`")
    lines.append(f"- risk_filter: `{risk_filter or 'all'}`")
    lines.append("")

    render_adapter_status_markdown(lines, observability)

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

    render_observability_sections_markdown(lines, observability)
    return "\n".join(lines) + "\n"


def resolve_output_format(output_format: str, out_file: Path) -> str:
    if output_format in {"markdown", "html"}:
        return output_format
    return "html" if out_file.suffix.lower() == ".html" else "markdown"


def _render_html_table(headers: list[str], rows: list[list[str]]) -> str:
    header_html = "".join(f"<th>{html.escape(h)}</th>" for h in headers)
    body_html = []
    for row in rows:
        cols = "".join(f"<td>{html.escape(c)}</td>" for c in row)
        body_html.append(f"<tr>{cols}</tr>")
    return (
        '<table border="1" cellpadding="6" cellspacing="0">'
        f"<thead><tr>{header_html}</tr></thead>"
        f"<tbody>{''.join(body_html)}</tbody>"
        "</table>"
    )


def render_dashboard_html(
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
    observability: dict[str, Any] | None = None,
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
    if observability is None:
        observability = {
            "adapter_status": [],
            "routing_quality": {"no_data": True, "series": [], "summary": {}},
            "autonomous_completion": {"no_data": True, "series": [], "summary": {}},
            "ci_self_heal": {"no_data": True, "series": [], "summary": {}},
            "safety_events": {"no_data": True, "series": [], "summary": {}},
            "memory_health": {"no_data": True, "series": [], "summary": {}},
            "data_lineage": [],
        }

    now = dt.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    parts: list[str] = [
        "<!doctype html>",
        "<html lang='en'><head><meta charset='utf-8'>",
        "<meta name='viewport' content='width=device-width, initial-scale=1'>",
        "<title>Sparks KPI + Eval Dashboard</title>",
        "<style>body{font-family:ui-sans-serif,system-ui,sans-serif;max-width:1200px;margin:24px auto;padding:0 16px;line-height:1.4}h1,h2,h3{margin-top:28px}table{width:100%;border-collapse:collapse;margin:10px 0 20px}th,td{border:1px solid #d0d7de;padding:6px;text-align:left}code{background:#f6f8fa;padding:1px 4px;border-radius:4px}ul{margin-top:8px}</style>",
        "</head><body>",
        "<h1>Sparks KPI + Eval Dashboard</h1>",
        "<ul>",
        f"<li>generated_utc: <code>{html.escape(now)}</code></li>",
        f"<li>repo: <code>{html.escape(repo_name)}</code></li>",
        f"<li>lane_filter: <code>{html.escape(lane_filter or 'all')}</code></li>",
        f"<li>risk_filter: <code>{html.escape(risk_filter or 'all')}</code></li>",
        "</ul>",
    ]

    smoke = latest_matching(history, lambda h: is_smoke_suite(str(h.get("suite", ""))))
    parts.append("<h2>Latest Smoke Eval (Health)</h2>")
    if smoke:
        parts.append("<ul>")
        parts.append(f"<li>suite: <code>{html.escape(str(smoke.get('suite', 'unknown')))}</code></li>")
        parts.append(
            f"<li>timestamp_utc: <code>{html.escape(str(smoke.get('timestamp_utc', 'unknown')))}</code></li>"
        )
        parts.append(f"<li>gate: <code>{'PASS' if smoke.get('gate_ok') else 'FAIL'}</code></li>")
        parts.append(f"<li>overall_score: <code>{float(smoke.get('overall_score', 0)):.2f}</code></li>")
        parts.append(f"<li>task_count: <code>{int(smoke.get('task_count', 0))}</code></li>")
        parts.append("</ul>")
    else:
        parts.append("<p>no smoke eval history found</p>")

    real = latest_matching(history, lambda h: not is_smoke_suite(str(h.get("suite", ""))))
    parts.append("<h2>Latest Real Eval (Quality Gate)</h2>")
    if real:
        parts.append("<ul>")
        parts.append(f"<li>suite: <code>{html.escape(str(real.get('suite', 'unknown')))}</code></li>")
        parts.append(
            f"<li>timestamp_utc: <code>{html.escape(str(real.get('timestamp_utc', 'unknown')))}</code></li>"
        )
        parts.append(f"<li>gate: <code>{'PASS' if real.get('gate_ok') else 'FAIL'}</code></li>")
        parts.append(f"<li>overall_score: <code>{float(real.get('overall_score', 0)):.2f}</code></li>")
        parts.append(f"<li>task_count: <code>{int(real.get('task_count', 0))}</code></li>")
        parts.append("</ul>")
    else:
        parts.append("<p>no real quality-gate eval history found</p>")

    parts.append("<h2>Eval Trend (All Suites)</h2>")
    eval_rows: list[list[str]] = []
    if history:
        for h in history:
            eval_rows.append(
                [
                    str(h.get("timestamp_utc", "?")),
                    str(h.get("suite", "?")),
                    "PASS" if h.get("gate_ok") else "FAIL",
                    f"{float(h.get('overall_score', 0)):.2f}",
                    str(int(h.get("task_count", 0))),
                    f"{float(h.get('exec_success_rate', 0)):.2f}",
                ]
            )
    else:
        eval_rows.append(["-", "-", "-", "-", "-", "-"])
    parts.append(
        _render_html_table(
            ["timestamp", "suite", "gate", "overall", "tasks", "exec_success_rate"],
            eval_rows,
        )
    )

    parts.append("<h2>KPI Trend (Snapshot History)</h2>")
    parts.append("<ul>")
    parts.append(
        f"<li>task_success_trend: <code>{html.escape(summarize_rate_trend(kpi_trend, 'task_success_rate'))}</code></li>"
    )
    parts.append(
        f"<li>verification_trend: <code>{html.escape(summarize_rate_trend(kpi_trend, 'verification_pass_rate'))}</code></li>"
    )
    parts.append(
        f"<li>rollback_trend: <code>{html.escape(summarize_rate_trend(kpi_trend, 'rollback_rate'))}</code></li>"
    )
    parts.append("</ul>")
    kpi_rows: list[list[str]] = []
    if kpi_trend:
        for r in kpi_trend:
            kpi_rows.append(
                [
                    str(r["captured_at"]),
                    str(r["lane"]),
                    str(r["risk"]),
                    f"{float(r['task_success_rate']) * 100.0:.1f}%",
                    f"{float(r['verification_pass_rate']) * 100.0:.1f}%",
                    f"{float(r['rollback_rate']) * 100.0:.1f}%",
                    str(r["tasks_started"]),
                ]
            )
    else:
        kpi_rows.append(["-", "-", "-", "n/a", "n/a", "n/a", "0"])
    parts.append(
        _render_html_table(
            ["captured_at", "lane", "risk", "task_success", "verification", "rollback", "started"],
            kpi_rows,
        )
    )

    parts.append("<h2>Current KPI Snapshot (from outcomes)</h2>")
    kpi_snapshot_rows: list[list[str]] = []
    if kpis:
        for r in kpis:
            kpi_snapshot_rows.append(
                [
                    str(r["lane"]),
                    str(r["risk"]),
                    str(r["started"]),
                    str(r["succeeded"]),
                    str(r["failed"]),
                    str(r["task_success_rate"]),
                    str(r["verification_pass_rate"]),
                    str(r["rollback_rate"]),
                    str(r["mean_time_to_fix"]),
                ]
            )
    else:
        kpi_snapshot_rows.append(["-", "-", "0", "0", "0", "n/a", "n/a", "n/a", "n/a"])
    parts.append(
        _render_html_table(
            ["lane", "risk", "started", "succeeded", "failed", "task_success", "verification", "rollback", "mttf"],
            kpi_snapshot_rows,
        )
    )

    parts.append("<h2>Token Cost (Estimated)</h2>")
    parts.append("<ul>")
    parts.append(
        f"<li>pricing_version: <code>{html.escape(str(token_cost_summary.get('pricing_version_used', DEFAULT_PRICING_VERSION)))}</code></li>"
    )
    parts.append(
        f"<li>total_tokens: <code>{int(token_cost_summary.get('total_tokens', 0))}</code></li>"
    )
    parts.append(
        f"<li>total_cost_usd: <code>{float(token_cost_summary.get('total_cost_usd', 0.0)):.6f}</code></li>"
    )
    parts.append("</ul>")
    provider_rows: list[list[str]] = []
    providers = token_cost_summary.get("providers", [])
    if providers:
        for p in providers:
            prompt_tokens = int(p.get("prompt_tokens", 0))
            completion_tokens = int(p.get("completion_tokens", 0))
            provider_rows.append(
                [
                    str(p.get("provider", "unknown")),
                    str(int(p.get("tasks", 0))),
                    str(prompt_tokens),
                    str(completion_tokens),
                    str(prompt_tokens + completion_tokens),
                    f"{float(p.get('cost_usd', 0.0)):.6f}",
                    "known" if p.get("known_pricing", False) else "unknown",
                ]
            )
    else:
        provider_rows.append(["-", "0", "0", "0", "0", "0.000000", "n/a"])
    parts.append("<h3>Aggregate by Provider</h3>")
    parts.append(
        _render_html_table(
            ["provider", "tasks", "prompt_tokens", "completion_tokens", "total_tokens", "cost_usd", "pricing"],
            provider_rows,
        )
    )
    parts.append("<h3>Per-Task Cost (Recent)</h3>")
    task_rows: list[list[str]] = []
    if token_cost_rows:
        for r in token_cost_rows[: max(1, task_cost_limit)]:
            task_rows.append(
                [
                    str(r["timestamp_utc"]),
                    str(r["suite"]),
                    str(r["task_id"]),
                    str(r["provider"]),
                    str(r["prompt_tokens"]),
                    str(r["completion_tokens"]),
                    str(r["total_tokens"]),
                    f"{float(r['cost_usd']):.6f}",
                    "known" if r["known_pricing"] else "unknown",
                ]
            )
    else:
        task_rows.append(["-", "-", "-", "-", "0", "0", "0", "0.000000", "n/a"])
    parts.append(
        _render_html_table(
            ["timestamp", "suite", "task", "provider", "prompt", "completion", "total", "cost_usd", "pricing"],
            task_rows,
        )
    )

    parts.append("<h2>Per-Ghost Performance</h2>")
    parts.append(f"<p>sample_threshold: <code>&gt;= {max(1, ghost_min_samples)}</code></p>")
    ghost_rows: list[list[str]] = []
    if ghost_breakdown:
        for row in ghost_breakdown:
            ghost_rows.append(
                [
                    str(row["ghost"]),
                    str(row["samples"]),
                    str(row["succeeded"]),
                    str(row["failed"]),
                    str(row["rolled_back"]),
                    str(row["success_rate"]),
                    str(row["sample_flag"]),
                ]
            )
    else:
        ghost_rows.append(["-", "0", "0", "0", "0", "n/a", "n/a"])
    parts.append(
        _render_html_table(
            ["ghost", "samples", "succeeded", "failed", "rolled_back", "success_rate", "sample_flag"],
            ghost_rows,
        )
    )

    render_observability_sections_html(parts, observability)

    parts.append("</body></html>")
    return "".join(parts) + "\n"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Render combined KPI + eval dashboard.")
    p.add_argument("--config", default="config.toml")
    p.add_argument("--repo", default="sparks")
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
    p.add_argument(
        "--routing-cohort-split",
        type=float,
        default=0.5,
        help="Chronological split for routing early/late cohorts (0.2-0.8).",
    )
    p.add_argument(
        "--output-format",
        choices=["auto", "markdown", "html"],
        default="auto",
        help="Output format; auto uses file extension (.html -> html, otherwise markdown).",
    )
    p.add_argument("--out-file", default="eval/results/dashboard.md")
    p.add_argument(
        "--json-out-file",
        default="eval/results/dashboard_data.json",
        help="Optional JSON export path for CI/release artifacts (empty disables).",
    )
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
    json_out_file = resolve(repo, args.json_out_file) if str(args.json_out_file).strip() else None

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
    observability = build_observability_bundle(
        conn=conn,
        history=history,
        history_source_exists=history_path.exists(),
        kpi_trend=kpi_trend,
        repo_name=args.repo,
        lane_filter=args.lane,
        risk_filter=args.risk,
        routing_cohort_split=args.routing_cohort_split,
    )
    conn.close()
    token_cost_rows, token_cost_summary = build_token_cost_rows(
        history,
        repo_root=repo,
        pricing_version=args.pricing_version,
    )

    output_format = resolve_output_format(args.output_format, out_file)
    if output_format == "html":
        content = render_dashboard_html(
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
            observability=observability,
        )
    else:
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
            observability=observability,
        )
    out_file.parent.mkdir(parents=True, exist_ok=True)
    out_file.write_text(content)
    if json_out_file is not None:
        json_out_file.parent.mkdir(parents=True, exist_ok=True)
        json_payload = {
            "generated_utc": dt.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ"),
            "repo": args.repo,
            "lane_filter": args.lane,
            "risk_filter": args.risk,
            "observability": observability,
            "token_cost_summary": token_cost_summary,
        }
        json_out_file.write_text(json.dumps(json_payload, indent=2, sort_keys=True))
        print(f"dashboard_json={json_out_file}")
    print(f"dashboard={out_file}")
    print(f"dashboard_format={output_format}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
