# Sparks Execution Contract v1

Standard runtime contract for coding CLI tools (`codex`, `claude_code`, `opencode`).

## Objective

Ensure deterministic execution behavior independent of CLI backend.

## Normalized Tool Envelope

Each execution must produce normalized fields:

- `tool`: cli backend name
- `task_id`: Sparks task ID
- `exit_code`: numeric process code
- `status`: `succeeded | failed | timeout | contract_error`
- `stdout`: captured standard output
- `stderr`: captured standard error
- `artifacts`: emitted artifact paths
- `started_at` and `ended_at`

## Deterministic Error Taxonomy

Use canonical reasons:

- `dispatch_timeout`
- `outcome_wait_timeout`
- `stale_started`
- `cli_contract_error`
- `cli_unavailable`
- `verify_failed`
- `test_failed`
- `policy_blocked`

## CLI Contract Marker

Wrapper errors should emit structured marker lines:

- `[sparks_cli_contract] tool=<tool> code=<code> retry_same=<true|false> fallback=<true|false> exit_code=<n|-> timeout_secs=<n>`

Consumers must parse this marker and apply policy deterministically.

## Retry and Fallback Policy

Policy inputs:

- error code
- risk tier
- attempt index
- configured backend availability

Policy outputs:

- retry same backend count
- fallback backend or none
- terminal vs retryable decision

Policy requirements:

- same inputs always produce same decision
- retries are bounded
- fallback order is explicit and configurable

## Artifact and Memory Guarantees

Every terminal run must emit:

- full artifact set (`request`, `plan`, `execution`, `diff`, `verify`, `outcome`)
- memory entries for success/failure with root cause and follow-up action

## Promotion Gate Inputs

A run is promotable only if:

- terminal status exists
- required verification checks passed
- eval gate passed
- promotion policy allows risk tier

## Guardrails

Hard bans for autonomous execution:

- direct secret exfiltration or secret printing
- destructive git commands
- direct edits to protected branches
- policy bypass attempts
