# Eval Smoke Runbook

Use this runbook for a fast integration check of Emberloom benchmark plumbing.

## Purpose

- verify harness scripts still execute end-to-end
- verify benchmark result files are generated
- catch wiring regressions before running expensive real-gate suites

## Prerequisites

- Python 3.11+
- Emberloom repo checkout
- optional: built `target/debug/athena` (not required for mock smoke)

## Local Smoke (Mock Dispatch)

```bash
python3 scripts/eval_harness.py \
  --suite eval/benchmark-mini-ci.json \
  --config config.example.toml \
  --athena-bin scripts/mock_athena_dispatch.py \
  --no-use-worktree \
  --history-file /tmp/athena-history.jsonl
```

Expected output artifacts:

- `eval/results/eval-<timestamp>.json`
- `eval/results/eval-<timestamp>.md`

## Script Regression Tests

```bash
python3 scripts/test_eval_harness.py
python3 scripts/test_optimizer_tournament.py
python3 scripts/test_generate_improvement_backlog.py
```

## CI Workflow

Hosted smoke CI job:

- `.github/workflows/eval-harness.yml`

## Escalate to Real Gate

When smoke passes and you need delivery-quality scoring:

```bash
python3 scripts/eval_harness.py --suite eval/benchmark-real-gate.json
```

Note: real-gate and nightly optimizer workflows use self-hosted runners by design.
