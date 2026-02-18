# Parsing Hardening Plan

Date: 2026-02-18

Goal: remove brittle string heuristics from manager/strategy execution paths and replace them with deterministic, test-backed parsing contracts.

## Status (Canonical)

Last updated: 2026-02-18  
Nightly update rule: update this checklist first; `docs/self-improvement-roadmap.md` mirrors this status.

- [x] Phase 1 (Complete): CLI execution contract parsing hardened (`[athena_cli_contract]` tags + parser regression tests).
- [ ] Phase 2 (In progress): manager direct-step fallback parsing hardening (`parse_direct_steps_from`) with malformed-input tests.
- [ ] Phase 3 (In progress): structured marker rollout for manager/strategy phase outputs to replace heuristic extraction.
- [ ] Phase 4 (In progress): retire legacy heuristic parsing paths after marker coverage + regression gates are green.
