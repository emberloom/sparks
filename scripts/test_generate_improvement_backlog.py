#!/usr/bin/env python3
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import generate_improvement_backlog as backlog


class ImprovementBacklogTests(unittest.TestCase):
    def test_normalize_failure_error_groups_timeout_variants(self) -> None:
        self.assertEqual(
            backlog.normalize_failure_error("dispatch_wait_timeout_after=120s"),
            "dispatch_timeout",
        )
        self.assertEqual(
            backlog.normalize_failure_error("eval_outcome_wait_timeout_after=2s"),
            "outcome_wait_timeout",
        )
        self.assertEqual(
            backlog.normalize_failure_error("stale_started_timeout"),
            "stale_started",
        )

    def test_build_ticket_sets_default_metadata(self) -> None:
        ticket = backlog.build_ticket(
            source="runtime_failures",
            title="x",
            evidence="y",
            impact=4,
            confidence=4,
            effort=3,
            risk="medium",
            acceptance=["a"],
        )
        self.assertEqual(ticket["status"], "open")
        self.assertEqual(ticket["owner"], "athena-runtime")
        self.assertEqual(ticket["eta"], "2d")


if __name__ == "__main__":
    unittest.main()
