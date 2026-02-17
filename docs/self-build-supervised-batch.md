# Self-Build Supervised Batch

Run a deterministic supervised batch of Phase 3 self-build tickets and emit a consolidated report.

## Inputs

- tickets file: one ticket per line
- optional example: `eval/self-build-supervised-tickets.example.txt`

## Preflight Checklist

- verify git working tree is clean
- verify athena binary is built
- verify gh auth status is valid

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
