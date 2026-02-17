#!/usr/bin/env python3
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


if __name__ == "__main__":
    unittest.main()
