# Sparks Task Contract v1

Use one task contract per executable task in a feature DAG.

## Metadata

- `task_id`: stable ID (for example `T3`)
- `feature_id`: parent feature contract ID
- `lane`: `delivery | self_improvement`
- `risk_tier`: `low | medium | high`
- `owner`: `<human or agent>`

## Objective

- One atomic change objective (single terminal definition of done).

## Inputs

- `context`: required background facts only
- `dependencies`: upstream task IDs that must be complete
- `linked_acceptance`: acceptance IDs this task satisfies

## Constraints

- `allowed_paths`: file/path allowlist
- `blocked_paths`: file/path denylist
- `tooling_limits`: allowed CLIs/tools
- `safety_rules`: no secret output, no destructive git, no protected-branch direct edits

## Execution Plan

1. Explore: read-only inspection and plan.
2. Execute: minimal patch for objective.
3. Verify: run required checks.
4. Emit: artifact and memory outputs.

## Verification Requirements

- `required_commands`: exact checks to run
- `expected_results`: pass/fail expectations
- `regression_scope`: what must not break

Example:

- `required_commands`:
  - `cargo test -p sparks --test test_eval_harness`
  - `python3 scripts/test_eval_harness.py`

## Done Criteria

- objective completed
- linked acceptance criteria advanced with evidence
- required checks passed
- terminal outcome emitted (`succeeded | failed | rolled_back`)

## Artifacts Required

- `request.json`
- `plan.md`
- `execution.log`
- `diff-summary.md`
- `verify-summary.md`
- `outcome.json`

## Failure Contract

If task fails, include:

- deterministic error code (taxonomy)
- root cause hypothesis
- next best patch task proposal
