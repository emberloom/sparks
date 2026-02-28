# Agents Guide

## What Athena Is
Athena is a security-first autonomous multi-agent system for shipping tasks safely. It turns user goals and external tickets into structured autonomous tasks, executes them in isolated environments, and records outcomes for health and KPI tracking. The mission contract is: run only the minimum necessary tools, keep credentials out of source control, and always prefer safe, auditable operations over speed.

## How Ghosts Work
Ghosts are sub-agents that execute tasks inside Docker containers with explicit tool allowlists and mount rules.

Key mechanics:
- **Isolation:** Ghosts run inside Docker images defined in config. Mounts are explicit and can be read-only.
- **Tool allowlists:** Each ghost is configured with an allowlist of tools. The manager enforces these per step.
- **Strategy phases:** The coding strategy uses EXPLORE → EXECUTE → VERIFY phases with different tool sets and step limits.
- **Resource limits:** Docker memory/CPU/time limits are enforced per run.

Relevant code: `src/docker.rs`, `src/manager.rs`, `src/strategy/code.rs`, `config.example.toml`.

## Security Model
Security is enforced across layers:
- **Tool gating:** The manager and tool layer restrict tool usage to allowlisted, validated operations.
- **Path validation:** File and shell tools validate paths and block sensitive locations.
- **Sensitive command patterns:** The manager rejects dangerous shell patterns (configurable in `manager.sensitive_patterns`).
- **Credential hygiene:** Inline secrets in config are blocked by default; secrets should be provided via env, `.env`, or OS keyring.
- **Risk tiers:** Every autonomous task is tagged with a risk tier (low/medium/high) for KPI attribution and safety gates.

Relevant code: `src/tools.rs`, `src/manager.rs`, `src/config.rs`, `src/doctor.rs`.

## Task Flow
```
Intake (poll/webhook)
  -> Dispatch (autonomous task queue)
    -> Execute (ghost strategy)
      -> Outcome (DB + memories)
        -> Sync (write-back to ticket source)
```

Relevant code: `src/ticket_intake/`, `src/core.rs`, `src/kpi.rs`.

## Contributing as an Agent
Before opening a PR or submitting a patch:
1. Run `cargo check` (or `cargo check --all-features` if touching optional features).
2. Run `cargo test` for relevant areas.
3. Run `athena doctor --ci` for safety and system checks.
4. Run `scripts/maintainability_check.py` if you changed core architecture or tool behavior.

Review expectations:
- No hardcoded credentials or tokens.
- No unsafe shell execution patterns.
- New traits should have safe defaults to avoid breaking downstream implementations.
- Database migrations are append-only and must be forward compatible.
- Keep risk tier defaults conservative.

## Risks and Mitigations
| Threat | Mitigation |
| --- | --- |
| Credential exfiltration | No inline secrets; keyring + env vars; path validation in tools. |
| Prompt injection | Tool allowlists, explicit confirmation flows, constrained strategies. |
| Privilege escalation | Docker isolation, limited mounts, sensitive command filters. |
| Data loss | No destructive git operations by default; guarded shell patterns. |

## Self-Improvement Loop
Athena continuously scans for health signals, memory gaps, and maintainability issues. Background loops collect tool usage stats, store health alerts/fixes, and periodically re-index code for refactoring opportunities.

Relevant code: `src/proactive/`, `src/self_heal.rs`, `src/kpi.rs`.

## Key References
- Core orchestration: `src/core.rs`
- Ghosts and strategy phases: `src/strategy/`
- Tool sandboxing and validation: `src/tools.rs`
- Ticket intake and sync: `src/ticket_intake/`
- Configuration and security: `src/config.rs`, `config.example.toml`
- Diagnostics: `src/doctor.rs`
