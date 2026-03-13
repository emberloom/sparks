# Feature Contract Workflow

Date: 2026-03-03

## Purpose

Run multi-task feature delivery with explicit dependency DAGs and auditable run artifacts.

## 1. Author Contract

Create a template with defaults:

```bash
sparks feature init --file feature-contract.toml --pattern fanout-fanin
```

Alternative pattern:

```bash
sparks feature init --file feature-contract-linear.toml --pattern linear
```

Accepted input formats for runtime commands:
- TOML
- YAML
- JSON

## 2. Validate/Lint (Fail Fast)

```bash
sparks feature validate --file feature-contract.toml
# alias:
sparks feature lint --file feature-contract.toml
```

Validation exits non-zero on any contract error and prints deterministic diagnostics with field paths, cycle traces, and unknown-ID suggestions.

Example diagnostics:

```text
Invalid feature contract (my-feature):
- tasks[2].depends_on[0] references unknown task 'TSK-2' (did you mean 'TASK-2'?)
- task dependency graph contains a cycle among enabled tasks: T3 -> T2 -> T3. remaining_blocked_tasks=T2,T3
```

## 3. Inspect DAG Plan

```bash
sparks feature plan --file feature-contract.toml
```

This prints:
- DAG execution batches
- acceptance coverage
- verification-check coverage

## 4. Dispatch Tasks

```bash
sparks feature dispatch --file feature-contract.toml --wait-secs 240
```

Useful options:
- `--continue-on-failure`
- `--dry-run`
- `--outcome-grace-secs <n>`
- `--rollback-on-failure`
- `--cli-tool codex|claude_code|opencode`
- `--cli-model <model>`
- `--lane <delivery|self_improvement>`
- `--risk <low|medium|high>`
- `--repo <label>`

Dispatch emits:
- `eval/results/feature-<feature_id>-<timestamp>.json|md`
- `artifacts/feature-contract-report-<feature_id>.json|md`
- `eval/results/ci-monitor-<timestamp>-<pr>.json` (for tasks that open a GitHub PR when CI autopilot is enabled)

The contract report is emitted for both success and failure paths.

CI autopilot behavior for dispatch:
- Default is **enabled** via `[ticket_intake.ci_autopilot]` in config.
- For successful tasks that open a PR, Sparks monitors CI, attempts bounded self-heal, and records `ci_monitor_status` in dispatch/report artifacts.
- Non-green CI autopilot outcomes are treated as task failures in dispatch summaries.

## 5. Run Verification

```bash
sparks feature verify --file feature-contract.toml --profile strict
```

Profile behavior:
- `fast`: only checks with `profile: fast`
- `strict`: checks with `profile: fast|strict`

Verify emits:
- `eval/results/feature-verify-<feature_id>-<timestamp>.json|md`

## 6. Gate (Dispatch + Verify + Promote)

```bash
sparks feature gate \
  --file feature-contract.toml \
  --wait-secs 240 \
  --verify-profile strict
```

Gate emits:
- dispatch ledger
- verify ledger
- promotion decision
- consolidated gate ledger
- contract run report (`artifacts/feature-contract-report-<feature_id>.json|md`)

## 7. Promote (Supervised Decision)

```bash
sparks feature promote --file feature-contract.toml
```

Optional explicit ledgers:

```bash
sparks feature promote \
  --file feature-contract.toml \
  --dispatch-ledger eval/results/feature-<feature_id>-<timestamp>.json \
  --verify-ledger eval/results/feature-verify-<feature_id>-<timestamp>.json
```

## Migration Guide

If you have older workflows:
1. Replace `--contract` with `--file` when convenient (`--contract` still works as an alias).
2. Existing YAML/JSON files continue to work.
3. For TOML adoption, start with `feature init` and copy your existing DAG/check definitions.
