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
    conn.execute(
        """
        CREATE TABLE memories (
            id TEXT PRIMARY KEY,
            category TEXT NOT NULL,
            content TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            embedding BLOB
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE ticket_intake_log (
            dedup_key TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            external_id TEXT NOT NULL,
            title TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'dispatched',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            issue_number TEXT,
            ci_monitor_status TEXT
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
            repo_name="sparks",
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
            ("delivery", "sparks", "low", "2026-03-01 01:00:00", 0.50, 0.60, 0.10, 10, 5, 5),
            ("delivery", "sparks", "low", "2026-03-01 03:00:00", 0.70, 0.80, 0.05, 12, 9, 3),
            ("delivery", "sparks", "medium", "2026-03-01 02:00:00", 0.55, 0.65, 0.12, 8, 4, 4),
            ("self_improvement", "sparks", "low", "2026-03-01 02:00:00", 0.90, 0.95, 0.01, 6, 6, 0),
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
            repo_name="sparks",
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
            repo_name="sparks",
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
            VALUES (?, 'delivery', 'sparks', 'low', ?, 'goal', ?, datetime('now','-1 minute'), datetime('now'), ?)
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
            repo_name="sparks",
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
            repo_name="sparks",
            lane_filter="delivery",
            risk_filter="low",
            ghost_min_samples=3,
        )
        self.assertIn("sparse data: no ghosts meet the stable-sample threshold yet", content)
        self.assertIn("`low-sample(<3)`", content)

    def test_routing_cohort_trend_computation(self) -> None:
        route_adapter = {
            "rows": [
                {"timestamp_utc": "2026-03-01T01:00:00Z", "bucket": "2026-03-01", "success": True},
                {"timestamp_utc": "2026-03-01T02:00:00Z", "bucket": "2026-03-01", "success": False},
                {"timestamp_utc": "2026-03-02T01:00:00Z", "bucket": "2026-03-02", "success": True},
                {"timestamp_utc": "2026-03-02T02:00:00Z", "bucket": "2026-03-02", "success": True},
            ],
            "source_missing": False,
            "schema_warnings": [],
        }
        outcomes_adapter = {"rows": [], "source_missing": True, "schema_warnings": []}
        trend = eval_dashboard.compute_routing_quality_trend(
            route_adapter=route_adapter,
            outcomes_adapter=outcomes_adapter,
            cohort_split=0.5,
        )
        self.assertFalse(trend["no_data"])
        self.assertAlmostEqual(trend["summary"]["early_rate"], 0.5)
        self.assertAlmostEqual(trend["summary"]["late_rate"], 1.0)
        self.assertAlmostEqual(trend["summary"]["delta"], 0.5)

    def test_autonomous_completion_and_first_pass_verify_metrics(self) -> None:
        outcomes_adapter = {
            "source": "sqlite.autonomous_task_outcomes",
            "source_missing": False,
            "schema_warnings": [],
            "rows": [
                {"bucket": "2026-03-01", "status": "succeeded", "verification_total": 1, "verification_passed": 1},
                {"bucket": "2026-03-01", "status": "failed", "verification_total": 1, "verification_passed": 0},
                {"bucket": "2026-03-02", "status": "succeeded", "verification_total": 2, "verification_passed": 2},
                {"bucket": "2026-03-02", "status": "succeeded", "verification_total": 0, "verification_passed": 0},
            ],
        }
        trend = eval_dashboard.compute_autonomous_completion_trend(outcomes_adapter)
        self.assertFalse(trend["no_data"])
        self.assertAlmostEqual(trend["summary"]["latest_completion_rate"], 0.75)
        self.assertAlmostEqual(trend["summary"]["latest_first_pass_verify_rate"], 2.0 / 3.0)

    def test_ci_self_heal_rollback_aggregation(self) -> None:
        self_heal = {
            "rows": [
                {"bucket": "2026-03-01", "success": True},
                {"bucket": "2026-03-02", "success": False},
            ],
            "source_missing": False,
            "schema_warnings": [],
        }
        outcomes = {
            "rows": [
                {"bucket": "2026-03-01", "status": "succeeded", "rolled_back": 0},
                {"bucket": "2026-03-01", "status": "failed", "rolled_back": 1},
                {"bucket": "2026-03-02", "status": "succeeded", "rolled_back": 0},
            ],
            "source_missing": False,
            "schema_warnings": [],
        }
        ci = {
            "rows": [
                {"bucket": "2026-03-01", "timestamp_utc": "2026-03-01T05:00:00Z", "status": "pass", "status_class": "pass"},
                {"bucket": "2026-03-02", "timestamp_utc": "2026-03-02T05:00:00Z", "status": "failed", "status_class": "fail"},
            ],
            "source_missing": False,
            "schema_warnings": [],
        }
        trend = eval_dashboard.compute_ci_self_heal_rollbacks(self_heal, outcomes, ci)
        self.assertFalse(trend["no_data"])
        self.assertEqual(trend["summary"]["self_heal_attempts"], 2)
        self.assertEqual(trend["summary"]["self_heal_successes"], 1)
        self.assertEqual(trend["summary"]["rollbacks"], 1)
        self.assertEqual(trend["summary"]["ci_pass"], 1)
        self.assertEqual(trend["summary"]["ci_fail"], 1)

    def test_build_observability_bundle_and_lineage_presence(self) -> None:
        conn = _setup_conn()
        conn.executemany(
            """
            INSERT INTO autonomous_task_outcomes (
                task_id, lane, repo, risk_tier, ghost, goal, status, started_at, finished_at,
                verification_total, verification_passed, rolled_back, error
            ) VALUES (?, 'delivery', 'sparks', 'low', 'coder', 'goal', ?, ?, ?, ?, ?, ?, ?)
            """,
            [
                ("o1", "succeeded", "2026-03-01 01:00:00", "2026-03-01 01:05:00", 1, 1, 0, ""),
                ("o2", "failed", "2026-03-01 02:00:00", "2026-03-01 02:05:00", 1, 0, 1, "safety policy denied"),
                ("o3", "succeeded", "2026-03-02 01:00:00", "2026-03-02 01:05:00", 2, 2, 0, ""),
                ("o4", "succeeded", "2026-03-02 02:00:00", "2026-03-02 02:05:00", 0, 0, 0, ""),
            ],
        )
        conn.executemany(
            """
            INSERT INTO memories (id, category, content, active, created_at, updated_at)
            VALUES (?, ?, ?, 1, ?, ?)
            """,
            [
                ("m1", "route_outcome", "route_id=r1 route_type=direct ghost=coder status=succeeded elapsed_ms=100", "2026-03-01 01:00:00", "2026-03-01 01:00:00"),
                ("m2", "route_outcome", "route_id=r2 route_type=direct ghost=coder status=failed elapsed_ms=120", "2026-03-01 02:00:00", "2026-03-01 02:00:00"),
                ("m3", "route_outcome", "route_id=r3 route_type=complex ghost=coder status=succeeded elapsed_ms=130", "2026-03-02 01:00:00", "2026-03-02 01:00:00"),
                ("m4", "route_outcome", "route_id=r4 route_type=complex ghost=coder status=succeeded elapsed_ms=140", "2026-03-02 02:00:00", "2026-03-02 02:00:00"),
                ("m5", "self_heal_outcome", json.dumps({"error_category": "type_error", "success": True}), "2026-03-01 03:00:00", "2026-03-01 03:00:00"),
                ("m6", "self_heal_outcome", json.dumps({"error_category": "import_error", "success": False}), "2026-03-02 03:00:00", "2026-03-02 03:00:00"),
                ("m7", "health_alert", "alert_kinds=error_rate", "2026-03-01 04:00:00", "2026-03-01 04:00:00"),
                ("m8", "health_fix", "fixed alert_kinds=error_rate", "2026-03-02 04:00:00", "2026-03-02 04:00:00"),
                ("m9", "pattern", "memory pattern", "2026-03-02 05:00:00", "2026-03-02 05:00:00"),
            ],
        )
        conn.executemany(
            """
            INSERT INTO ticket_intake_log (
                dedup_key, provider, external_id, title, status, created_at, ci_monitor_status
            ) VALUES (?, 'linear', ?, ?, 'dispatched', ?, ?)
            """,
            [
                ("d1", "ext1", "Issue 1", "2026-03-01 06:00:00", "ci_pass"),
                ("d2", "ext2", "Issue 2", "2026-03-02 06:00:00", "ci_fail"),
            ],
        )
        conn.commit()

        observability = eval_dashboard.build_observability_bundle(
            conn=conn,
            history=[],
            history_source_exists=False,
            kpi_trend=[],
            repo_name="sparks",
            lane_filter="delivery",
            risk_filter="low",
            routing_cohort_split=0.5,
        )
        self.assertFalse(observability["routing_quality"]["no_data"])
        self.assertFalse(observability["autonomous_completion"]["no_data"])
        self.assertFalse(observability["ci_self_heal"]["no_data"])
        self.assertFalse(observability["safety_events"]["no_data"])
        self.assertFalse(observability["memory_health"]["no_data"])
        self.assertGreaterEqual(len(observability["data_lineage"]), 5)

        md = eval_dashboard.render_dashboard(
            history=[],
            kpis=[],
            kpi_trend=[],
            ghost_breakdown=[],
            repo_name="sparks",
            observability=observability,
        )
        self.assertIn("Routing Quality Trend (Early vs Late Cohorts)", md)
        self.assertIn("Data Lineage", md)

    def test_missing_source_behavior_renders_no_data_explicitly(self) -> None:
        conn = sqlite3.connect(":memory:")
        observability = eval_dashboard.build_observability_bundle(
            conn=conn,
            history=[],
            history_source_exists=False,
            kpi_trend=[],
            repo_name="sparks",
            lane_filter=None,
            risk_filter=None,
            routing_cohort_split=0.5,
        )
        content = eval_dashboard.render_dashboard(
            history=[],
            kpis=[],
            kpi_trend=[],
            ghost_breakdown=[],
            repo_name="sparks",
            observability=observability,
        )
        self.assertIn("Source Adapter Status", content)
        self.assertIn("no routing quality data available", content)
        self.assertIn("no autonomous completion trend data available", content)
        self.assertIn("Data Lineage", content)

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
            repo_name="sparks",
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
        self.assertIn("Routing Quality Trend (Early vs Late Cohorts)", html_text)
        self.assertIn("Autonomous Completion &amp; First-pass Verify", html_text)
        self.assertIn("CI Self-heal / Rollback Outcomes", html_text)
        self.assertIn("Safety Events", html_text)
        self.assertIn("Memory Health Signals", html_text)
        self.assertIn("Data Lineage", html_text)


if __name__ == "__main__":
    unittest.main()
