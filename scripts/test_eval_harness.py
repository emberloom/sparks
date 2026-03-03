#!/usr/bin/env python3
import json
import sqlite3
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_harness


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
    return conn


class EvalHarnessRegressionTests(unittest.TestCase):
    def test_to_text_handles_bytes(self) -> None:
        self.assertEqual(eval_harness._to_text(b"hello"), "hello")
        self.assertEqual(eval_harness._to_text(None), "")

    def test_fail_outcome_if_started_only_updates_started(self) -> None:
        conn = _setup_conn()
        conn.execute(
            """
            INSERT INTO autonomous_task_outcomes
            (task_id, lane, repo, risk_tier, goal, status, started_at)
            VALUES ('t-started','delivery','athena','low','goal','started',datetime('now'))
            """
        )
        conn.execute(
            """
            INSERT INTO autonomous_task_outcomes
            (task_id, lane, repo, risk_tier, goal, status, started_at, finished_at)
            VALUES ('t-done','delivery','athena','low','goal','failed',datetime('now','-10 sec'),datetime('now'))
            """
        )
        conn.commit()

        self.assertTrue(
            eval_harness.fail_outcome_if_started(
                conn, "t-started", eval_harness.OUTCOME_REASON_WAIT_TIMEOUT
            )
        )
        self.assertFalse(
            eval_harness.fail_outcome_if_started(
                conn, "t-done", eval_harness.OUTCOME_REASON_WAIT_TIMEOUT
            )
        )

        row1 = conn.execute(
            "SELECT status, error, finished_at FROM autonomous_task_outcomes WHERE task_id='t-started'"
        ).fetchone()
        row2 = conn.execute(
            "SELECT status, error FROM autonomous_task_outcomes WHERE task_id='t-done'"
        ).fetchone()
        self.assertEqual(row1[0], "failed")
        self.assertEqual(row1[1], eval_harness.OUTCOME_REASON_WAIT_TIMEOUT)
        self.assertIsNotNone(row1[2])
        self.assertEqual(row2[0], "failed")
        self.assertIsNone(row2[1])

    def test_wait_for_terminal_outcome_timeout_then_finalize(self) -> None:
        conn = _setup_conn()
        task_id = "t-timeout"
        conn.execute(
            """
            INSERT INTO autonomous_task_outcomes
            (task_id, lane, repo, risk_tier, goal, status, started_at)
            VALUES (?1,'delivery','athena','low','goal','started',datetime('now'))
            """,
            (task_id,),
        )
        conn.commit()

        outcome, terminal = eval_harness.wait_for_terminal_outcome(
            conn, task_id, max_wait_secs=0, poll_secs=0.01
        )
        self.assertFalse(terminal)
        self.assertEqual(outcome.get("status"), "started")

        changed = eval_harness.fail_outcome_if_started(
            conn, task_id, eval_harness.OUTCOME_REASON_WAIT_TIMEOUT
        )
        self.assertTrue(changed)
        final = eval_harness.query_outcome(conn, task_id)
        self.assertEqual(final.get("status"), "failed")
        self.assertEqual(final.get("error"), eval_harness.OUTCOME_REASON_WAIT_TIMEOUT)

    def test_evaluate_gate_strict_delivery_rules_fail_on_low_tests(self) -> None:
        results = [
            eval_harness.TaskResult(
                task_id="t1",
                lane="delivery",
                risk="low",
                ghost="coder",
                cli_tool=None,
                cli_model=None,
                dispatch_task_id="id-1",
                status="succeeded",
                error=None,
                exec_success=1.0,
                plan_quality=0.7,
                tests_pass=0.0,
                diff_quality=1.0,
                overall=0.8,
                changed_files=[],
                stdout="",
                stderr="",
                notes=[],
            )
        ]
        suite = {
            "gate_requirements": {
                "min_overall": 0.7,
                "require_exec_success": True,
                "lane_rules": {"delivery": {"min_tests_pass": 1.0, "min_diff_quality": 0.8}},
            }
        }
        ok, reasons = eval_harness.evaluate_gate(results, threshold=0.7, suite=suite, overall=0.8)
        self.assertFalse(ok)
        self.assertTrue(any("tests_pass<1.00" in r for r in reasons))

    def test_evaluate_gate_strict_delivery_rules_pass_when_thresholds_met(self) -> None:
        results = [
            eval_harness.TaskResult(
                task_id="t1",
                lane="delivery",
                risk="low",
                ghost="coder",
                cli_tool=None,
                cli_model=None,
                dispatch_task_id="id-1",
                status="succeeded",
                error=None,
                exec_success=1.0,
                plan_quality=0.7,
                tests_pass=1.0,
                diff_quality=0.9,
                overall=0.85,
                changed_files=[],
                stdout="",
                stderr="",
                notes=[],
            )
        ]
        suite = {
            "gate_requirements": {
                "min_overall": 0.7,
                "require_exec_success": True,
                "lane_rules": {"delivery": {"min_tests_pass": 1.0, "min_diff_quality": 0.8}},
            }
        }
        ok, reasons = eval_harness.evaluate_gate(results, threshold=0.7, suite=suite, overall=0.85)
        self.assertTrue(ok)
        self.assertEqual(reasons, [])

    def test_score_plan_quality_structured_json(self) -> None:
        payload = {
            "plan": ["step one", "step two"],
            "execution": ["do work"],
            "verification": "run tests",
            "rollback_plan": ["git reset --hard HEAD~1"],
        }
        score = eval_harness.score_plan_quality(json.dumps(payload))
        self.assertAlmostEqual(score, 1.0)

    def test_score_plan_quality_fallback_legacy(self) -> None:
        response = "\n".join(
            [
                "PLAN:",
                "- step one",
                "- step two",
                "EXECUTION:",
                "- run command",
                "- verify tests",
            ]
        )
        score = eval_harness.score_plan_quality(response)
        self.assertAlmostEqual(score, 1.0)

    def test_parse_dispatch_task_id_picks_first_match(self) -> None:
        stderr = (
            "task_id=11111111-1111-1111-1111-111111111111 "
            "task_id=22222222-2222-2222-2222-222222222222"
        )
        self.assertEqual(
            eval_harness.parse_dispatch_task_id(stderr),
            "11111111-1111-1111-1111-111111111111",
        )

    def test_parse_dispatch_task_id_rejects_malformed_uuid(self) -> None:
        stderr = "task_id=not-a-uuid"
        self.assertIsNone(eval_harness.parse_dispatch_task_id(stderr))

    def test_parse_dispatch_task_id_returns_none_when_missing(self) -> None:
        stderr = "no task id here"
        self.assertIsNone(eval_harness.parse_dispatch_task_id(stderr))


if __name__ == "__main__":
    unittest.main()
