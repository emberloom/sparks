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
- `--cli-tool codex|claude_code|opencode`
- `--cli-model <model>`
- `--lane <delivery|self_improvement>`
- `--risk <low|medium|high>`
- `--repo <label>`

Dispatch behavior:

- tasks run in topological order
- tasks with failed/skipped dependencies are skipped
- each task waits for terminal outcome correlation via `task_id`
- timeouts are finalized with canonical reasons (`dispatch_timeout`, `outcome_wait_timeout`)
- acceptance coverage and satisfaction are summarized in per-run ledgers:
  - `eval/results/feature-<feature_id>-<timestamp>.json`
  - `eval/results/feature-<feature_id>-<timestamp>.md`

## Verify

```bash
athena feature verify --file eval/feature-contract-example.yaml
```

Verify behavior:

- runs each `verification_checks[].command` via shell (`zsh -lc`)
- records pass/fail with exit codes and output tails
- computes acceptance satisfaction from passing mapped checks
- emits ledgers:
  - `eval/results/feature-verify-<feature_id>-<timestamp>.json`
  - `eval/results/feature-verify-<feature_id>-<timestamp>.md`
- fails non-zero if promotion gate fails

Validation guarantees:

- every enabled task maps at least one acceptance criterion
- every mapped acceptance ID must exist in `acceptance_criteria`
- every acceptance criterion must be covered by at least one enabled task
