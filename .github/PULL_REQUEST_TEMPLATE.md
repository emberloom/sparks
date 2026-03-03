## Description

<!-- Describe what this PR does and why. Link to any relevant issues or context. -->

Closes #

## Type of Change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor / code cleanup
- [ ] Documentation
- [ ] CI / tooling

## Pre-PR Checklist

- [ ] `cargo check -q` passes with no warnings
- [ ] `cargo check -q --features telegram` passes with no warnings (if touching feature-gated code)
- [ ] `cargo test -q` passes (284+ tests)
- [ ] `python3 scripts/dead_code_check.py --telegram` — zero dead code
- [ ] `python3 scripts/wiring_check.py` — all 10 wiring checks pass
- [ ] `python3 scripts/hygiene_check.py` — hygiene baseline not regressed
- [ ] `CHANGELOG.md` updated under `[Unreleased]` (skip for pure docs/tooling changes)

## Related Issues / PRs

<!-- List any related issues or PRs here -->
