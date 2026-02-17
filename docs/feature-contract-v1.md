# Athena Feature Contract v1

Use this contract before decomposing a feature into implementation tasks.

## Metadata

- `feature_id`: `<repo>-<short-name>-<yyyy-mm-dd>`
- `owner`: `<human or agent>`
- `lane`: `delivery | self_improvement`
- `risk_tier`: `low | medium | high`
- `status`: `draft | approved | in_progress | done | archived`

## Outcome

- `user_problem`: one paragraph
- `target_outcome`: measurable expected behavior/result
- `non_goals`: explicit exclusions

## Constraints

- `scope_paths`: allowed files/directories
- `forbidden_ops`: destructive or policy-blocked actions
- `dependencies`: upstream systems/apis this feature depends on
- `security_constraints`: secrets/data handling boundaries

## Acceptance Criteria

Define atomic criteria with stable IDs.

- `AC-1`: ...
- `AC-2`: ...
- `AC-3`: ...

Each criterion must be objectively testable.

## Architecture and Interfaces

- `design_notes`: brief design rationale
- `interface_contracts`: API/schema/event changes with compatibility plan
- `migration_plan`: if storage/schema behavior changes

## Task DAG

Define tasks with explicit dependencies.

| task_id | summary | depends_on | risk | mapped_acceptance |
|---|---|---|---|---|
| T1 | Define API contract | - | low | AC-1 |
| T2 | Implement backend | T1 | medium | AC-1, AC-2 |
| T3 | Implement frontend | T1 | medium | AC-2 |
| T4 | End-to-end tests | T2, T3 | low | AC-1, AC-2, AC-3 |

Rules:

- DAG only (no cycles).
- No task without at least one mapped acceptance criterion.
- No acceptance criterion without at least one owning task.

## Verification Plan

- `unit_tests`: list
- `integration_tests`: list
- `manual_checks`: list
- `performance_or_reliability_checks`: list

Runtime shape for `athena feature verify`:

- `verification_checks[]`:
  - `id`
  - `command`
  - `profile` (`fast | strict`, default `strict`)
  - `mapped_acceptance[]`
  - `required` (default `true`)

## Promotion Policy

- `low risk`: auto-merge allowed only when gates are green and confidence is high
- `medium/high risk`: PR-only, human approval required

Runtime decision command:

- `athena feature promote --file <contract>`

One-shot gate command:

- `athena feature gate --file <contract> --verify-profile strict`

## Evidence Ledger

Record evidence per acceptance criterion.

| acceptance_id | evidence_type | location | result |
|---|---|---|---|
| AC-1 | test | `<path or artifact>` | pass/fail |
| AC-2 | benchmark | `<path or artifact>` | pass/fail |
| AC-3 | review | `<path or artifact>` | approved/changes_requested |
