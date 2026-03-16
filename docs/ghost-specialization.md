# Spark Specialization Policy

Emberloom can auto-select autonomous task sparks using KPI history instead of a static default.

## Scope

Policy is evaluated per autonomous task scope:
- `repo`
- `lane`
- `risk_tier`

If a task specifies an explicit spark, specialization is bypassed.

## Baseline Selection

Baseline default is:
- `coder` when present
- otherwise first configured spark

## Decision Inputs

Policy combines:
- overall spark metrics
- recent-window spark metrics
- previous selected spark

Metrics include:
- success rate
- verification pass rate
- rollback rate
- task sample count

## Default Thresholds

Current runtime thresholds:
- `min_samples = 3`
- `confidence_threshold = 0.05`
- `rollback_min_samples = 3`
- `max_allowed_regression = 0.08`
- `stability_window = 3`

## Decision Actions

- `keep_default`
- `promote { candidate }`
- `rollback { to_baseline }`

Selection mode is emitted as telemetry (`promote`, `rollback`, `fallback_default`, or explicit/fallback paths).

## Telemetry

Specialization decisions are logged with:
- selected spark
- scope (`lane`, `repo`, `risk`)
- sample count
- success rate
- confidence gap
- rationale reason codes

## Current Boundary

Specialization is runtime policy-based for dispatch routing.
It does not automatically materialize repo-specific spark config files.
