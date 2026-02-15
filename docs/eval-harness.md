# Athena Eval Harness

Date: 2026-02-15

## Purpose

Fixed benchmark gate for "is Athena getting better?".

It runs a stable task suite end-to-end and scores:

- plan quality
- execution success
- tests pass
- diff quality

## Suite

Default suite file:

- `eval/benchmark-suite.json`

Current lanes covered:

- `delivery`
- `self_improvement`

## Run

```bash
python3 scripts/eval_harness.py --suite eval/benchmark-suite.json --config config.toml --athena-bin target/debug/athena
```

Optional:

```bash
python3 scripts/eval_harness.py --fail-fast
```

Recommended isolation mode (default):

- each task runs in a disposable git worktree
- no direct edits to your current working tree

Disable if needed:

```bash
python3 scripts/eval_harness.py --no-use-worktree
```

## Output

Reports are written to:

- `eval/results/eval-<timestamp>.json`
- `eval/results/eval-<timestamp>.md`
- `eval/results/history.jsonl` (append-only trend history)

And console prints gate result:

- `gate=PASS` or `gate=FAIL`

## Gate Rule

Gate passes when:

- overall weighted score `>= pass_threshold` (suite-level)
- all tasks have `exec_success = 1.0`

## Notes

- Default behavior uses disposable worktrees per task.
- Keep benchmark tasks stable and explicit.
- Prefer small, real backlog tasks with clear acceptance criteria.
- Task outcome scoring waits for terminal statuses (`succeeded|failed|rolled_back`) with polling.

## Dashboard

Render combined KPI + eval trend dashboard:

```bash
python3 scripts/eval_dashboard.py --config config.toml --repo athena
```

Output:

- `eval/results/dashboard.md`
- includes KPI snapshot with: task success rate, verification pass rate, rollback rate, and mean time to fix (MTTF)
