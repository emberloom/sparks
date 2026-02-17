# Self-Build Supervised Batch

Run a deterministic supervised batch of Phase 3 self-build tickets and emit a consolidated report.

## Inputs

- tickets file: one ticket per line
- optional example: `eval/self-build-supervised-tickets.example.txt`

## Dry Run (Preparation)

```bash
python3 scripts/supervised_self_build_batch.py \
  --tickets-file eval/self-build-supervised-tickets.example.txt \
  --promote-mode pr \
  --cli-tool codex \
  --dry-run
```

## Real Supervised Batch

```bash
python3 scripts/supervised_self_build_batch.py \
  --tickets-file eval/self-build-supervised-tickets.example.txt \
  --risk low \
  --wait-secs 300 \
  --maintenance-profile rust \
  --promote-mode pr \
  --base-branch main \
  --cli-tool codex
```

By default the runner pre-builds `target/debug/athena` before executing tickets.
Use `--skip-build` only when you intentionally want to reuse an existing binary.
`athena self-build run` now retries one time automatically when dispatch reports success but leaves an empty git diff.

## Outputs

- per run:
  - `eval/results/self-build-*.json/.md`
  - `eval/results/self-build-review-*.json/.md`
- batch summary:
  - `eval/results/self-build-batch-*.json/.md`
  - latest pointers:
    - `eval/results/self-build-batch-latest.json`
    - `eval/results/self-build-batch-latest.md`

## Promotion Safety Defaults

- use `--promote-mode pr` for supervised batches
- reserve `--promote-mode auto` for explicitly approved low-risk experiments
- inspect `self_build_policy` line and checklist blockers before merging
