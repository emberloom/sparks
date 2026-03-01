#!/usr/bin/env python3
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_dashboard


def _setup_conn() -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    conn.execute(
        """
        CREATE TABLE autonomous_task_outcomes (
            task_id TEXT PRIMARY KEY,
            lane TEXT,
            repo TEXT,
            risk_tier TEXT,
            ghost TEXT,
            goal TEXT,
            status TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            verification_total INTEGER DEFAULT 0,
            verification_passed INTEGER DEFAULT 0,
            rolled_back INTEGER DEFAULT 0,
            error TEXT
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE kpi_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            lane TEXT NOT NULL,
            repo TEXT NOT NULL,
            risk_tier TEXT NOT NULL,
            captured_at TEXT NOT NULL,
            task_success_rate REAL NOT NULL,
            verification_pass_rate REAL NOT NULL,
            rollback_rate REAL NOT NULL,
            mean_time_to_fix_secs REAL,
            tasks_started INTEGER NOT NULL,
            tasks_succeeded INTEGER NOT NULL,
            tasks_failed INTEGER NOT NULL,
            verifications_total INTEGER NOT NULL,
            verifications_passed INTEGER NOT NULL,
            rollbacks INTEGER NOT NULL
        )
        """
    )
    return conn


class EvalDashboardTests(unittest.TestCase):
    def test_render_dashboard_handles_missing_kpi_trend(self) -> None:
        content = eval_dashboard.render_dashboard(
            history=[],
            kpis=[],
            kpi_trend=[],
            ghost_breakdown=[],
            repo_name="athena",
            lane_filter="delivery",
            risk_filter="high",
            ghost_min_samples=3,
        )
        self.assertIn("no KPI snapshot trend found for current filters", content)
        self.assertIn("| - | - | - | n/a | n/a | n/a | 0 |", content)
        self.assertIn("- lane_filter: `delivery`", content)
        self.assertIn("- risk_filter: `high`", content)

    def test_query_kpi_snapshot_trend_filters_and_orders(self) -> None:
        conn = _setup_conn()
        rows = [
            ("delivery", "athena", "low", "2026-03-01 01:00:00", 0.50, 0.60, 0.10, 10, 5, 5),
            ("delivery", "athena", "low", "2026-03-01 03:00:00", 0.70, 0.80, 0.05, 12, 9, 3),
            ("delivery", "athena", "medium", "2026-03-01 02:00:00", 0.55, 0.65, 0.12, 8, 4, 4),
            ("self_improvement", "athena", "low", "2026-03-01 02:00:00", 0.90, 0.95, 0.01, 6, 6, 0),
            ("delivery", "other", "low", "2026-03-01 02:00:00", 0.33, 0.40, 0.25, 3, 1, 2),
        ]
        conn.executemany(
            """
            INSERT INTO kpi_snapshots (
                lane, repo, risk_tier, captured_at,
                task_success_rate, verification_pass_rate, rollback_rate,
                mean_time_to_fix_secs,
                tasks_started, tasks_succeeded, tasks_failed,
                verifications_total, verifications_passed, rollbacks
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, 0, 0, 0)
            """,
            rows,
        )
        conn.commit()

        trend = eval_dashboard.query_kpi_snapshot_trend(
            conn,
            repo_name="athena",
            lane_filter="delivery",
            risk_filter="low",
            limit=20,
        )
        self.assertEqual(len(trend), 2)
        self.assertEqual(trend[0]["captured_at"], "2026-03-01 01:00:00")
        self.assertEqual(trend[1]["captured_at"], "2026-03-01 03:00:00")

        content = eval_dashboard.render_dashboard(
            history=[],
            kpis=[],
            kpi_trend=trend,
            ghost_breakdown=[],
            repo_name="athena",
            lane_filter="delivery",
            risk_filter="low",
            ghost_min_samples=3,
        )
        self.assertIn("50.0% -> 70.0% (up", content)
        self.assertIn("| `2026-03-01 03:00:00` | `delivery` | `low` | 70.0% | 80.0% | 5.0% | 12 |", content)

    def test_ghost_breakdown_sparse_data_indicator(self) -> None:
        conn = _setup_conn()
        conn.executemany(
            """
            INSERT INTO autonomous_task_outcomes
            (task_id, lane, repo, risk_tier, ghost, goal, status, started_at, finished_at, rolled_back)
            VALUES (?, 'delivery', 'athena', 'low', ?, 'goal', ?, datetime('now','-1 minute'), datetime('now'), ?)
            """,
            [
                ("t1", "coder", "succeeded", 0),
                ("t2", "coder", "failed", 0),
                ("t3", "scout", "succeeded", 0),
            ],
        )
        conn.commit()

        ghosts = eval_dashboard.query_ghost_breakdown(
            conn,
            repo_name="athena",
            lane_filter="delivery",
            risk_filter="low",
            min_samples=3,
        )
        self.assertEqual(len(ghosts), 2)
        self.assertTrue(all(not g["meets_threshold"] for g in ghosts))
        self.assertEqual(ghosts[0]["sample_flag"], "low-sample(<3)")

        content = eval_dashboard.render_dashboard(
            history=[],
            kpis=[],
            kpi_trend=[],
            ghost_breakdown=ghosts,
            repo_name="athena",
            lane_filter="delivery",
            risk_filter="low",
            ghost_min_samples=3,
        )
        self.assertIn("sparse data: no ghosts meet the stable-sample threshold yet", content)
        self.assertIn("`low-sample(<3)`", content)

    def test_lookup_pricing_unknown_provider_and_version_fallback(self) -> None:
        in_price, out_price, used_version, known = eval_dashboard.lookup_pricing(
            provider="unknown",
            pricing_version="v999",
        )
        self.assertEqual(used_version, eval_dashboard.DEFAULT_PRICING_VERSION)
        self.assertFalse(known)
        self.assertEqual(in_price, 0.0)
        self.assertEqual(out_price, 0.0)

    def test_build_token_cost_rows_infers_pricing_and_fallbacks(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            report = root / "eval-report.json"
            report.write_text(
                json.dumps(
                    {
                        "suite": "smoke-mini",
                        "results": [
                            {
                                "task_id": "t-known",
                                "cli_model": "openai/gpt-5-codex",
                                "stdout": "prompt_tokens=120 completion_tokens=30",
                                "stderr": "",
                            },
                            {
                                "task_id": "t-unknown",
                                "stdout": "",
                                "stderr": "",
                                "prompt_tokens": 50,
                                "completion_tokens": 25,
                            },
                        ],
                    }
                )
            )
            history = [
                {
                    "timestamp_utc": "2026-03-01T10:00:00Z",
                    "suite": "smoke-mini",
                    "report_json": str(report),
                }
            ]
            rows, summary = eval_dashboard.build_token_cost_rows(
                history=history,
                repo_root=root,
                pricing_version="v999",
            )

        self.assertEqual(summary["pricing_version_used"], eval_dashboard.DEFAULT_PRICING_VERSION)
        self.assertEqual(len(rows), 2)
        self.assertTrue(any(r["provider"] == "openai" and r["known_pricing"] for r in rows))
        self.assertTrue(any(r["provider"] == "unknown" and not r["known_pricing"] for r in rows))
        self.assertGreater(summary["total_cost_usd"], 0.0)
        self.assertGreaterEqual(summary["unknown_pricing_tasks"], 1)

    def test_resolve_output_format_auto(self) -> None:
        self.assertEqual(
            eval_dashboard.resolve_output_format("auto", Path("/tmp/dashboard.md")),
            "markdown",
        )
        self.assertEqual(
            eval_dashboard.resolve_output_format("auto", Path("/tmp/dashboard.html")),
            "html",
        )
        self.assertEqual(
            eval_dashboard.resolve_output_format("html", Path("/tmp/dashboard.md")),
            "html",
        )

    def test_render_dashboard_html_contains_required_sections(self) -> None:
        html_text = eval_dashboard.render_dashboard_html(
            history=[],
            kpis=[],
            kpi_trend=[],
            ghost_breakdown=[],
            repo_name="athena",
            token_cost_rows=[],
            token_cost_summary={
                "pricing_version_requested": "v1",
                "pricing_version_used": "v1",
                "total_cost_usd": 0.0,
                "total_cost_known_provider_usd": 0.0,
                "total_prompt_tokens": 0,
                "total_completion_tokens": 0,
                "total_tokens": 0,
                "unknown_provider_tasks": 0,
                "unknown_pricing_tasks": 0,
                "providers": [],
            },
        )
        self.assertIn("<!doctype html>", html_text.lower())
        self.assertIn("KPI Trend (Snapshot History)", html_text)
        self.assertIn("Per-Ghost Performance", html_text)
        self.assertIn("Token Cost (Estimated)", html_text)


if __name__ == "__main__":
    unittest.main()
