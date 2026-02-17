# Feature Contract Workflow

Date: 2026-02-17

## Purpose

Run multi-task feature delivery with explicit DAG dependencies using `athena feature`.

## Contract File

Use YAML or JSON with:

- `feature_id`
- optional defaults: `lane`, `risk`, `repo`
- `acceptance_criteria[]` (required):
  - `id`
  - optional `description`
- `verification_checks[]` (recommended, required for `feature verify`):
  - `id`
  - `command`
  - optional `profile` (`fast` or `strict`, default `strict`)
  - `mapped_acceptance[]`
  - optional `required` (default `true`)
- `tasks[]` with:
  - `id`, `goal`
  - required `mapped_acceptance[]` (one or more acceptance IDs)
  - optional `depends_on[]`, `ghost`, `context`
  - optional `lane`, `risk`, `repo` overrides
  - optional `wait_secs`, `auto_store`, `cli_tool`, `cli_model`
  - optional `enabled` (default `true`)

Example file:

- `eval/feature-contract-example.yaml`

## Validate

```bash
athena feature validate --file eval/feature-contract-example.yaml
```

## Plan

```bash
athena feature plan --file eval/feature-contract-example.yaml
```

This prints batch groups derived from DAG levels.
It also prints acceptance coverage (`acceptance_id -> covered_by task IDs`).

## Dispatch

```bash
athena feature dispatch --file eval/feature-contract-example.yaml --wait-secs 240
```

Useful options:

- `--continue-on-failure` keep running independent tasks
- `--dry-run` resolve DAG and print execution plan without dispatching
- `--outcome-grace-secs <n>` override adaptive DB terminal-outcome grace wait
- `--cli-tool codex|claude_code|opencode`
- `--cli-model <model>`
- `--lane <delivery|self_improvement>`
- `--risk <low|medium|high>`
- `--repo <label>`

Dispatch behavior:

- tasks run in topological order
- tasks with failed/skipped dependencies are skipped
- each task waits for terminal outcome correlation via `task_id`
- if pulse wait times out, dispatch performs an adaptive DB terminal-outcome grace wait before finalizing
- adaptive grace defaults by risk and task profile (ghost/goal/wait timeout), but can be overridden with `--outcome-grace-secs`
- unresolved waits are finalized with canonical reasons (`outcome_wait_timeout`, `dispatch_channel_closed`)
- acceptance coverage and satisfaction are summarized in per-run ledgers:
  - `eval/results/feature-<feature_id>-<timestamp>.json`
  - `eval/results/feature-<feature_id>-<timestamp>.md`

## Verify

```bash
athena feature verify --file eval/feature-contract-example.yaml --profile strict
```

Profile behavior:

- `--profile fast` runs only checks tagged `profile: fast`
- `--profile strict` runs both `fast` and `strict` checks

Verify behavior:

- runs each `verification_checks[].command` via shell (`zsh -lc`)
- records pass/fail with exit codes and output tails
- computes acceptance satisfaction from passing mapped checks
- emits ledgers:
  - `eval/results/feature-verify-<feature_id>-<timestamp>.json`
  - `eval/results/feature-verify-<feature_id>-<timestamp>.md`
- fails non-zero if promotion gate fails

## Gate (Dispatch + Verify + Promote)

```bash
athena feature gate \
  --file eval/feature-contract-example.yaml \
  --wait-secs 240 \
  --verify-profile strict
```

Gate behavior:

- runs dispatch flow and emits dispatch ledger
- runs verify flow with selected verification profile
- computes promotion decision using risk-tier policy
- emits consolidated gate artifacts:
  - `eval/results/feature-gate-<feature_id>-<timestamp>.json`
  - `eval/results/feature-gate-<feature_id>-<timestamp>.md`
- exits non-zero when `gate_ok=false`

## Promote (Supervised Decision)

```bash
athena feature promote --file eval/feature-contract-example.yaml
```

Optional explicit ledgers:

```bash
athena feature promote \
  --file eval/feature-contract-example.yaml \
  --dispatch-ledger eval/results/feature-<feature_id>-<timestamp>.json \
  --verify-ledger eval/results/feature-verify-<feature_id>-<timestamp>.json
```

Promote behavior:

- loads dispatch + verify ledgers (latest for feature by default)
- applies risk-tier policy:
  - `low`: auto-promote allowed only when both ledgers are promotable
  - `medium/high`: always approval-required (PR-only)
- emits decision artifacts:
  - `eval/results/feature-promote-<feature_id>-<timestamp>.json`
  - `eval/results/feature-promote-<feature_id>-<timestamp>.md`

Validation guarantees:

- every enabled task maps at least one acceptance criterion
- every mapped acceptance ID must exist in `acceptance_criteria`
- every acceptance criterion must be covered by at least one enabled task
