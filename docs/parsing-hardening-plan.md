# Parsing Hardening Plan (Mistake #4)

Date: 2026-02-17
Owner: Athena core
Scope: Replace heuristic LLM/CLI parsing paths with structured contracts and deterministic fallbacks.

## Status (2026-02-26)

- [ ] Phase 0 — baseline telemetry counters + snapshot artifact
- [~] Phase 1 — CLI contract parsing hardening (marker-anywhere + tests done; shared parser + param-validation markers pending)
- [ ] Phase 2 — classifier contract schema + structured error codes
- [~] Phase 3 — strategy text fallback normalization (strict JSON envelope done; repair turn + reason taxonomy pending)
- [~] Phase 4 — eval harness structured plan scoring (JSON scoring + legacy fallback done; artifact-based scoring pending)
- [ ] Phase 5 — strict parsing rollout + gates

## Why this is needed

Current parsing still relies on brittle heuristics in high-leverage orchestration paths:

- classifier safety-net checks string fragments (`"tool"` + `"params"`) in `src/manager.rs:817` and `src/manager.rs:835`
- CLI failure policy parser only inspects the first output line in `src/strategy/code.rs:966`
- eval plan-quality scoring still depends on text headings (`PLAN:`, `EXECUTION:`) in `scripts/eval_harness.py:236`
- fallback loops in strategy text mode still depend on JSON extraction from free-form text in `src/strategy/react.rs:298` and `src/strategy/mod.rs:162`

Observed signal:

- classifier heuristic warnings are present in runtime logs (`athena_stderr.log` has 3 `raw response contains tool JSON` warnings on 2026-02-12)
- execution contract document already requires structured markers, but not all consumers enforce them end-to-end (`docs/execution-contract-v1.md:37`)

## Target state

All critical parse boundaries are schema-first and versioned:

1. Athena does not infer control intent from prose when a contract can be required.
2. Every CLI/tool orchestration decision is based on parsed structured markers.
3. Eval scoring consumes structured artifacts, not response format heuristics.
4. Parse failures emit deterministic reason codes and telemetry.

## Implementation plan

### Phase 0: Baseline + instrumentation (1 day)

1. Add parse telemetry counters:
   - `parse.classifier.fallback`
   - `parse.cli_contract.missing`
   - `parse.cli_contract.invalid`
   - `parse.eval.plan_scoring.heuristic_used`
2. Emit counters to:
   - structured logs
   - Langfuse trace tags/metadata when enabled
3. Snapshot baseline in a single artifact:
   - `eval/results/parsing-baseline-<timestamp>.json`

Acceptance:

- one benchmark run produces non-empty parse telemetry fields
- no behavior changes yet

### Phase 1: CLI contract parser hardening (1-2 days)

1. Create a shared parser module (for Rust consumers) for contract lines:
   - locate marker anywhere in output, not only line 1
   - parse known keys deterministically (`tool`, `code`, `retry_same`, `fallback`, `exit_code`, `timeout_secs`)
   - return typed struct + parse errors
2. Replace `parse_cli_failure_policy()` in `src/strategy/code.rs:966` with shared parser.
3. Ensure all CLI tool failures emit marker format, including early guard-returns:
   - nested Claude session path in `src/tools.rs:1397`
   - parameter validation failures from `build_cli_prompt()` paths
4. Add tests:
   - marker first/middle/last line
   - noisy prefix/suffix
   - malformed bool/int fields
   - missing marker defaults with explicit reason code

Acceptance:

- deterministic parse tests pass in Rust unit tests
- no fallback decision should depend on free-text matching once marker exists

### Phase 2: Classifier contract schema (2 days)

1. Introduce explicit `ClassificationEnvelopeV1` enum with serde:
   - `simple { answer }`
   - `direct { steps[] }`
   - `complex { ghost, goal, context }`
2. Validate envelope and return structured parse error codes:
   - `classifier_contract_missing`
   - `classifier_contract_invalid`
   - `classifier_contract_semantic_invalid`
3. Remove string-fragment intent detection in `src/manager.rs:817` and `src/manager.rs:835` after migration.
4. Add prompt instruction update so orchestrator must return only envelope JSON.
5. Add manager unit tests for:
   - valid envelopes
   - invalid/missing fields
   - unknown tool in direct steps
   - backward compatibility for legacy keys (`agent` alias)

Acceptance:

- classifier behavior is decided by typed envelope parsing
- heuristic `"tool"`/`"params"` string checks are removed or dead code behind temporary flag

### Phase 3: Strategy fallback parsing normalization (2 days)

1. For text fallback loops in `src/strategy/react.rs` and `src/strategy/mod.rs`, require one of:
   - native tool call path (preferred), or
   - structured tool envelope (`tool`, `params`, `contract_version`)
2. Add one deterministic repair turn when parse fails:
   - system asks model to re-emit valid envelope only
   - bounded retry count = 1
3. Add reason taxonomy for terminal failure when parsing still fails:
   - `tool_contract_parse_failed`
4. Add fixtures for mixed prose + JSON, multiple JSON blobs, malformed JSON.

Acceptance:

- text fallback cannot execute tools from ambiguous prose
- failures produce deterministic parse reason codes

### Phase 4: Eval harness scoring contract (1-2 days)

1. Move plan-quality scoring from heading heuristics (`PLAN:`/`EXECUTION:`) to structured artifact:
   - read plan artifact emitted by strategy/executor
   - score based on explicit fields (steps, verification plan, rollback plan)
2. Keep legacy scorer behind temporary fallback flag for transition.
3. Add regression tests in `scripts/test_eval_harness.py` for both contract and fallback paths.

Acceptance:

- benchmark scoring is format-agnostic for equivalent plan content
- heuristic path usage is measurable and trendable to near-zero

### Phase 5: Rollout and gate tightening (1 day)

1. Enable strict parsing in benchmark and self-build lanes.
2. Add non-regression checks:
   - parse error rate must not increase
   - delivery success must not regress beyond tolerance
3. Promote strict mode to default after 3 consecutive non-regression runs.

Acceptance:

- strict parsing default is on
- fallback mode remains as emergency rollback only

## Test and validation matrix

Run after each phase:

1. `cargo test parse_cli_failure_policy -- --nocapture`
2. `cargo test parse_dispatch_task_id_extracts_uuid -- --nocapture`
3. `python3 scripts/test_eval_harness.py`
4. `python3 scripts/eval_harness.py --suite eval/benchmark-suite.json --config config.toml --athena-bin target/debug/athena --output-dir eval/results --history-file eval/results/history.jsonl`

Post-rollout checks:

1. Compare parse telemetry baseline vs latest run.
2. Confirm classifier heuristic warning count trends to zero in runtime logs.
3. Confirm promotion decisions still follow existing real-gate policy.

## Expected improvements

Yes, this should produce meaningful improvements.

Primary expected gains:

1. Lower nondeterministic routing/fallback decisions by replacing prose heuristics with typed contracts.
2. Higher reproducibility in CLI fallback behavior (same output -> same policy decision).
3. More stable eval signal by removing presentation-sensitive scoring.
4. Faster debugging via explicit parse reason taxonomy and telemetry.

Quantitative targets (first 2 weeks after rollout):

1. `parse.*` failure counters reduced by >=70% from baseline.
2. classifier heuristic warning count reduced to ~0 on new runs.
3. `unclassified` CLI policy outcomes reduced to <=5% of CLI failures.
4. no regression in real-gate pass rate relative to pre-rollout baseline.

## Dependencies and risks

Dependencies:

- stable CLI wrapper marker format in `src/tools.rs`
- benchmark runs available on self-hosted runner for comparison

Risks:

1. Over-strict schema can increase false negatives initially.
2. Existing prompts may require adjustment to emit strict envelopes consistently.
3. During migration, dual-path parsing can mask defects if fallback is too permissive.

Mitigation:

- ship phased with feature flags
- keep one bounded repair retry
- keep explicit telemetry on fallback usage and fail closed only after stability window
