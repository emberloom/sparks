# Optimizer Tournament (Phase 4)

Run a deterministic candidate tournament over a fixed benchmark suite and emit winner-selection artifacts.

## Purpose

- compare baseline vs mutated dispatch-context candidates
- enforce non-regression gates before candidate selection
- emit provenance for winner and promotion recommendation

## Command

```bash
python3 scripts/optimizer_tournament.py \
  --suite eval/benchmark-real-gate.json \
  --cli-tool codex \
  --max-candidates 5 \
  --top-backlog 2
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
- candidate mutations (built-in baseline + mutation variants)
- optional backlog hypotheses from `eval/results/improvement-backlog-latest.json`

## Outputs

- `eval/results/optimizer-tournament-*.json`
- `eval/results/optimizer-tournament-*.md`
- latest pointers:
  - `eval/results/optimizer-tournament-latest.json`
  - `eval/results/optimizer-tournament-latest.md`

## Selection Rules

- each candidate must pass regression gates versus baseline:
  - eval harness exit code `== 0`
  - `gate_ok=true`
  - score/exec deltas above configured regression floor (`--max-regression`)
- winner is highest `(overall_score, exec_success_rate, avg_task_overall)` among gate-passing candidates
- promotion is recommended only if winner beats baseline by `--min-improvement`
