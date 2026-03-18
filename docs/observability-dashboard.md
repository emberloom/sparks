# Observability Dashboard

Emberloom ships a static dashboard generator at `scripts/eval_dashboard.py`.
It renders from local artifacts only (SQLite + JSONL) and produces CI-friendly exports.

## Local Generation

Render markdown + JSON:

```bash
python3 scripts/eval_dashboard.py \
  --config config.toml \
  --repo sparks \
  --out-file eval/results/dashboard.md \
  --json-out-file eval/results/dashboard_data.json
```

Render HTML:

```bash
python3 scripts/eval_dashboard.py \
  --config config.toml \
  --repo sparks \
  --output-format html \
  --out-file eval/results/dashboard.html \
  --json-out-file eval/results/dashboard_data.json
```

Optional routing cohort boundary:

```bash
--routing-cohort-split 0.5
```

`0.5` means chronological halves (early cohort vs late cohort).

## Inputs and Schema Expectations

Primary sources:

- `eval/results/history.jsonl` (or `--history-file`) for eval suite outcomes
- SQLite DB path from `[db].path` in config

Relevant tables:

- `autonomous_task_outcomes`
- `kpi_snapshots`
- `memories`
- `ticket_intake_log`

The generator uses adapter normalization with explicit status reporting:

- `source_missing=true` when a source table/file is absent
- `schema_warnings` when expected fields are missing or malformed

Missing sources are rendered explicitly as `no data` sections, never silently omitted.

## Data Lineage Mapping

The dashboard includes a `Data Lineage` section with metric-level provenance:

| Metric | Source | Transform | Chart |
|---|---|---|---|
| Routing quality (early vs late) | `memories(route_outcome)` fallback `autonomous_task_outcomes` | status -> success flag, split by chronological cohort, aggregate daily rates | Routing Quality Trend (Early vs Late Cohorts) |
| Autonomous completion | `autonomous_task_outcomes(status,timestamps)` | daily succeeded / terminal tasks | Autonomous Completion & First-pass Verify |
| First-pass verify | `autonomous_task_outcomes(verification_total,verification_passed,status)` | daily fully-verified successes / verified tasks | Autonomous Completion & First-pass Verify |
| CI self-heal outcomes | `memories(self_heal_outcome)` | parse JSON success flag, aggregate attempts/successes | CI Self-heal / Rollback Outcomes |
| Rollback outcomes | `autonomous_task_outcomes(rolled_back,status)` | aggregate rollback counts + rollback rate | CI Self-heal / Rollback Outcomes |
| Safety events | `autonomous_task_outcomes(error,status,rolled_back)` + `memories(health_alert)` | aggregate failures/rollbacks/keyword hits/alerts | Safety Events |
| Memory health signals | `memories(category,active,embedding,created_at)` | category activity trend + embedding coverage + fix/alert ratio | Memory Health Signals |

## Output Artifacts

Generated files:

- `eval/results/dashboard.md`
- `eval/results/dashboard.html`
- `eval/results/dashboard_data.json`

## CI and Release Wiring

Workflows generating and publishing artifacts:

- `.github/workflows/eval-harness.yml`
  - runs dashboard tests
  - renders markdown/html/json
  - uploads `eval-dashboard-smoke` artifact
- `.github/workflows/eval-real-gate.yml`
  - renders markdown/html/json after real gate run
  - uploads `eval-real-gate-dashboard` artifact
- `.github/workflows/release.yml`
  - renders markdown/html/json in release job
  - includes dashboard files in release assets

## Release Notes Usage

For release-note snippets, consume:

- `eval/results/dashboard.md` for human summary blocks
- `eval/results/dashboard_data.json` for machine-generated digests and downstream automation
