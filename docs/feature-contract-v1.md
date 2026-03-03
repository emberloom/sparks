# Athena Feature Contract v1

Feature contracts define an auditable DAG of autonomous tasks mapped to acceptance criteria.

Supported file formats:
- YAML (`.yaml`, `.yml`)
- JSON (`.json`)
- TOML (`.toml`)

## Schema

Top-level fields:
- `feature_id` (required)
- `lane` (optional): `delivery | self_improvement`
- `risk` (optional): `low | medium | high`
- `repo` (optional)
- `acceptance_criteria[]` (required)
- `verification_checks[]` (recommended; required for `feature verify`)
- `tasks[]` (required)

`acceptance_criteria[]`:
- `id` (required)
- `description` (optional)

`verification_checks[]`:
- `id` (required)
- `command` (required)
- `profile` (optional, default `strict`): `fast | strict`
- `mapped_acceptance[]` (required, at least one)
- `required` (optional, default `true`)

`tasks[]`:
- `id` (required)
- `goal` (required when `enabled=true`)
- `mapped_acceptance[]` (required when `enabled=true`)
- `depends_on[]` (optional)
- `ghost`, `context` (optional)
- `lane`, `risk`, `repo` (optional task-level overrides)
- `wait_secs`, `auto_store`, `cli_tool`, `cli_model` (optional)
- `enabled` (optional, default `true`)

## DAG and Validation Rules

Validation is strict and deterministic:
- Duplicate IDs are rejected (`acceptance`, `tasks`, `verification_checks`).
- Unknown references fail validation (`depends_on`, `mapped_acceptance`).
- Disabled dependency references are rejected.
- Self-dependencies are rejected.
- Enabled tasks must map at least one acceptance criterion.
- Every acceptance criterion must be covered by at least one enabled task.
- Task graph must be acyclic.

Cycle diagnostics include an explicit trace, for example:

```text
task dependency graph contains a cycle among enabled tasks: T4 -> T2 -> T4. remaining_blocked_tasks=T2,T4
```

Unknown ID diagnostics include nearest-ID suggestions when possible, for example:

```text
tasks[3].depends_on[0] references unknown task 'TSK-2' (did you mean 'TASK-2'?)
```

## CLI Authoring UX

Initialize a new TOML contract:

```bash
athena feature init --file feature-contract.toml --pattern fanout-fanin
```

Patterns:
- `linear`
- `fanout-fanin`

Validate (or lint) a contract:

```bash
athena feature validate --file feature-contract.toml
athena feature lint --file feature-contract.toml
```

Backward-compatible flag alias:
- `--contract` is accepted as an alias for `--file`.

## Report Artifact

`feature dispatch` and `feature gate` emit a deterministic contract-run report:

- `artifacts/feature-contract-report-<feature_id>.json`
- `artifacts/feature-contract-report-<feature_id>.md`

Report includes:
- Top-level summary (`succeeded`, `failed`, `blocked`, `skipped`, verification totals)
- Per-task status
- Per-task `ci_monitor_status` (when PR CI autopilot runs)
- Dependency blockers
- Attempts and retries
- Task-to-acceptance and task-to-check mapping
- Acceptance-level satisfaction rollup

## Migration Notes

For existing users:
1. Existing YAML/JSON contracts remain supported unchanged.
2. Existing `--contract` CLI usage still works (`--file` is canonical).
3. To migrate to TOML, run:
   - `athena feature init --file <new>.toml --pattern <linear|fanout-fanin>`
   - copy existing task/check content into the generated template.
4. For dependency or mapping errors, prefer fixing the referenced IDs directly; diagnostics now include exact field paths and suggestions.
