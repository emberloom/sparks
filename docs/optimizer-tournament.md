# Optimizer Tournament (Phase 4)

Run a deterministic candidate tournament over a fixed benchmark suite and emit winner-selection artifacts.

## Purpose

- compare baseline vs static + backlog-derived mutated dispatch-context candidates
- enforce non-regression gates before candidate selection
- emit provenance for winner and promotion execution decision

## Command

```bash
python3 scripts/optimizer_tournament.py \
  --suite eval/benchmark-real-gate.json \
  --cli-tool codex \
  --top-backlog 6 \
  --backlog-mutations-per-ticket 3 \
  --max-candidates 18 \
  --promote-profile
```

Optional fast smoke:

```bash
python3 scripts/optimizer_tournament.py \
  --suite eval/benchmark-real-gate.json \
  --cli-tool codex \
  --max-tasks 1 \
  --max-candidates 3
```

## Inputs

- benchmark suite (`--suite`)
- candidate mutations:
  - baseline from active profile (`eval/results/optimizer-profile.json`)
  - built-in static mutations
  - backlog-derived hypotheses from top ranked tickets
- optional backlog hypotheses from `eval/results/improvement-backlog-latest.json`

## Outputs

- `eval/results/optimizer-tournament-*.json`
- `eval/results/optimizer-tournament-*.md`
- active profile (only when promotion criteria pass and `--promote-profile` is enabled):
  - `eval/results/optimizer-profile.json`
- latest pointers:
  - `eval/results/optimizer-tournament-latest.json`
  - `eval/results/optimizer-tournament-latest.md`

## Selection Rules

- each candidate must pass regression gates versus baseline:
  - eval harness exit code `== 0`
  - `gate_ok=true`
  - score/exec/task deltas above configured regression floor (`--max-regression`)
- winner is highest `(overall_score, exec_success_rate, avg_task_overall)` among gate-passing candidates
- promotion is recommended only if:
  - winner beats baseline by `--min-improvement` (positive score delta)
  - and strict mode also requires non-negative exec/task deltas (`--strict-promotion`, default enabled)
- profile promotion is executed only when `--promote-profile` is set and above criteria hold

## Nightly Full Real-Gate

Nightly schedule is wired for self-hosted runners:

- workflow: `.github/workflows/optimizer-tournament-nightly.yml`
- cadence: daily at `03:15 UTC`
- behavior:
  - regenerates improvement backlog
  - runs full real-gate tournament (no `--max-tasks`)
  - promotes active profile only when non-regression gates + positive delta pass
