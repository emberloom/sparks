# Emberloom Eval Harness

Date: 2026-02-15

## Purpose

Fixed benchmark gate for "is Emberloom getting better?".

It runs a stable task suite end-to-end and scores:

- plan quality
- execution success
- tests pass
- diff quality

## Current Limits (2026-02-16)

- CI currently runs smoke via mock dispatch (`eval/benchmark-mini-ci.json`) to validate harness mechanics.
- CLI smoke benchmark (`eval/benchmark-cli-smoke.json`) is integration-focused and lightweight.
- Fast benchmark mode (`[benchmark_fast_cli]`) skips EXPLORE/VERIFY in strategy and is not a full-quality gate.

Roadmap and target gate model: `docs/self-improvement-roadmap.md`.

## Suite

Default suite file:

- `eval/benchmark-suite.json`
- real quality gate profile: `eval/benchmark-real-gate.json`
- smoke profile: `eval/benchmark-cli-smoke.json`

Current lanes covered:

- `delivery`
- `self_improvement`

CI smoke suite:

- `eval/benchmark-mini-ci.json` (uses `scripts/mock_sparks_dispatch.py`)

## Run

Preferred CLI entrypoint:

```bash
cargo run -- eval run --suite eval/benchmark-suite.json
```

Compare against a baseline scorecard:

```bash
cargo run -- eval compare \
  --baseline eval/results/eval-scorecard-baseline.json \
  --candidate eval/results/eval-scorecard-latest.json
```

Scenario library manifest (used in scorecard metadata):

- `eval/scenario-library-v1.json`

Raw harness invocation:

```bash
python3 scripts/eval_harness.py --suite eval/benchmark-suite.json --config config.toml --sparks-bin target/debug/sparks
```

Optional:

```bash
python3 scripts/eval_harness.py --fail-fast
```

Pin a specific coding CLI backend for this run:

```bash
python3 scripts/eval_harness.py --cli-tool codex
python3 scripts/eval_harness.py --cli-tool claude_code
python3 scripts/eval_harness.py --cli-tool opencode
```

Optional model override (applies to selected CLI tool):

```bash
python3 scripts/eval_harness.py --cli-tool codex --cli-model gpt-5-codex
```

Optional dispatch context and CLI timeout cap:

```bash
python3 scripts/eval_harness.py --cli-tool codex --dispatch-context "[benchmark_fast_cli]" --cli-timeout-secs 300
```

Recommended isolation mode (default):

- each task runs in a disposable git worktree
- no direct edits to your current working tree
- stale disposable worktrees are auto-cleaned on startup (default: older than 6 hours)

Disable if needed:

```bash
python3 scripts/eval_harness.py --no-use-worktree
```

Cleanup controls:

```bash
python3 scripts/eval_harness.py --no-cleanup-worktrees
python3 scripts/eval_harness.py --stale-worktree-hours 24
```

Quick per-CLI comparison loop:

```bash
for tool in codex claude_code opencode; do
  python3 scripts/eval_harness.py --cli-tool "$tool" || true
done
```

Or use the matrix runner (recommended for all 3 tools):

```bash
python3 scripts/eval_cli_matrix.py --suite eval/benchmark-cli-smoke.json
```

Matrix output:

- `eval/results/cli-matrix-<timestamp>.json`
- `eval/results/cli-matrix-<timestamp>.md`

## Overnight Soak

Run unattended reliability soak (doctor + 3-CLI matrix + KPI snapshots + dashboard + maintainability snapshot + ranked improvement backlog):

```bash
./scripts/start-soak-autonomy.sh 28800 1800 overnight8h
```

Arguments:

- arg1: duration seconds (`28800` = 8h)
- arg2: interval seconds (`1800` = 30m)
- arg3: run label

Launcher output includes:

- `session=<screen session>`
- `run_dir=<path>`
- `launch_log=<path>`

Inspect progress:

```bash
tail -f "<run_dir>/soak.log"
screen -ls | rg sparks_soak
```

On completion, a summary is generated at:

- `<run_dir>/summary.md`

## Real Quality Gate

Run real delivery-quality suite (separate from smoke health):

```bash
python3 scripts/eval_harness.py --suite eval/benchmark-real-gate.json
```

Use smoke for integration uptime and real suite for promotion decisions.
Real gate enforces strict per-task `delivery` minima (`tests_pass=1.0`, `diff_quality>=0.8`) in addition to suite overall threshold.

## Output

Reports are written to:

- `eval/results/eval-<timestamp>.json`
- `eval/results/eval-<timestamp>.md`
- `eval/results/history.jsonl` (append-only trend history)

And console prints gate result:

- `gate=PASS` or `gate=FAIL`

## Gate Rule

Gate passes when:

- suite-level threshold passes (`overall weighted score >= pass_threshold`)
- `require_exec_success` (if enabled) passes
- all configured `gate_requirements` pass, including optional lane/task minima such as:
  - minimum `tests_pass`
  - minimum `diff_quality`
  - minimum `plan_quality`
  - minimum per-task `overall`

## Notes

- Default behavior uses disposable worktrees per task.
- Keep benchmark tasks stable and explicit.
- Prefer small, real backlog tasks with clear acceptance criteria.
- Task outcome scoring waits for terminal statuses (`succeeded|failed|rolled_back`) with polling.

## Dashboard

Render combined KPI + eval trend dashboard:

```bash
cargo run -- dashboard --repo sparks
```

Equivalent script invocation (used by soak/gate scripts):

```bash
python3 scripts/eval_dashboard.py --config config.toml --repo sparks
```

Output:

- `eval/results/dashboard.md`
- includes KPI snapshot with: task success rate, verification pass rate, rollback rate, and mean time to fix (MTTF)

## CI

GitHub Actions workflow:

- `.github/workflows/eval-harness.yml`

Planned CI evolution:

- keep smoke job as quick regression sentinel
- real benchmark gate is wired as manual self-hosted workflow: `.github/workflows/eval-real-gate.yml`
