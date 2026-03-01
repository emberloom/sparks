#!/usr/bin/env python3
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import optimizer_tournament as opt


class OptimizerTournamentTests(unittest.TestCase):
    def test_parse_report_json_path_prefers_kv(self) -> None:
        output = "\n".join(
            [
                "something",
                "report_json=/tmp/one.json",
                "other=1",
                "report_json=/tmp/two.json",
            ]
        )
        self.assertEqual(opt.parse_report_json_path(output), "/tmp/two.json")

    def test_backlog_candidates_generate_multiple_mutations(self) -> None:
        payload = {
            "tickets": [
                {
                    "title": "Reduce recurring failure: dispatch_timeout",
                    "source": "runtime_failures",
                    "risk": "medium",
                    "score": 1.23,
                    "evidence": "10 failed outcomes",
                    "acceptance": ["Reproduce deterministically", "Add regression check"],
                }
            ]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "backlog.json"
            path.write_text(json.dumps(payload))
            candidates = opt.backlog_candidates(path, "[base]", top=1, mutations_per_ticket=3)
        self.assertEqual(len(candidates), 3)
        self.assertEqual(len({c.candidate_id for c in candidates}), 3)
        self.assertTrue(all(c.candidate_id.startswith("backlog_1_") for c in candidates))
        self.assertTrue(
            all("constraint_strictness" in c.mutation_dimensions for c in candidates)
        )
        self.assertTrue(all("soul_composition" in c.mutation_dimensions for c in candidates))

    def test_pick_winner_no_promotion_on_non_positive_delta(self) -> None:
        baseline = opt.CandidateResult(
            candidate_id="baseline",
            source="baseline",
            hypothesis="baseline",
            dispatch_context="[base]",
            mutation_dimensions={},
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.90,
            exec_success_rate=1.0,
            avg_task_overall=0.90,
            task_count=3,
            error=None,
        )
        lower = opt.CandidateResult(
            candidate_id="mutation",
            source="mutation",
            hypothesis="mutation",
            dispatch_context="[m]",
            mutation_dimensions={},
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.89,
            exec_success_rate=1.0,
            avg_task_overall=0.90,
            task_count=3,
            error=None,
        )
        winner, promote, _, _ = opt.pick_winner(
            [baseline, lower], min_improvement=0.01, max_regression=0.02, strict_promotion=True
        )
        self.assertEqual(winner.candidate_id, "baseline")
        self.assertFalse(promote)

    def test_pick_winner_promotes_on_positive_delta(self) -> None:
        baseline = opt.CandidateResult(
            candidate_id="baseline",
            source="baseline",
            hypothesis="baseline",
            dispatch_context="[base]",
            mutation_dimensions={},
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.90,
            exec_success_rate=0.9,
            avg_task_overall=0.90,
            task_count=3,
            error=None,
        )
        better = opt.CandidateResult(
            candidate_id="mutation",
            source="mutation",
            hypothesis="mutation",
            dispatch_context="[m]",
            mutation_dimensions={},
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.93,
            exec_success_rate=1.0,
            avg_task_overall=0.92,
            task_count=3,
            error=None,
        )
        winner, promote, _, gates = opt.pick_winner(
            [baseline, better], min_improvement=0.01, max_regression=0.02, strict_promotion=True
        )
        self.assertEqual(winner.candidate_id, "mutation")
        self.assertTrue(promote)
        self.assertTrue(any(g["candidate_id"] == "mutation" and g["gate_ok"] for g in gates))

    def test_validate_mutation_axes_rejects_unknown_values(self) -> None:
        with self.assertRaises(ValueError):
            opt.validate_mutation_axes("invalid", "minimal")
        with self.assertRaises(ValueError):
            opt.validate_mutation_axes("strict", "invalid")

    def test_generate_specialized_ghost_profiles_filters_incomplete(self) -> None:
        good = opt.CandidateResult(
            candidate_id="candidate_good",
            source="backlog:runtime_failures",
            hypothesis="good",
            dispatch_context="[ctx]",
            mutation_dimensions={
                "constraint_strictness": "strict",
                "soul_composition": "balanced",
            },
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.95,
            exec_success_rate=1.0,
            avg_task_overall=0.94,
            task_count=3,
            error=None,
        )
        bad = opt.CandidateResult(
            candidate_id="candidate_bad",
            source="backlog:runtime_failures",
            hypothesis="bad",
            dispatch_context="[ctx]",
            mutation_dimensions={"constraint_strictness": "strict"},
            command=[],
            exit_code=0,
            report_json=None,
            gate_ok=True,
            overall_score=0.90,
            exec_success_rate=1.0,
            avg_task_overall=0.90,
            task_count=3,
            error=None,
        )
        profiles = opt.generate_specialized_ghost_profiles([good, bad], "20260301T000000Z")
        self.assertEqual(len(profiles), 1)
        self.assertEqual(profiles[0]["source_candidate_id"], "candidate_good")


if __name__ == "__main__":
    unittest.main()
