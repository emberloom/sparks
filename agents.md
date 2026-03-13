# Agents Guide

## What Emberloom Is
Emberloom is a security-first autonomous multi-agent system for shipping tasks safely. It turns user goals and external tickets into structured autonomous tasks, executes them in isolated environments, and records outcomes for health and KPI tracking. The mission contract is: run only the minimum necessary tools, keep credentials out of source control, and always prefer safe, auditable operations over speed.

## How Sparks Work
Sparks are sub-agents that execute tasks inside Docker containers with explicit tool allowlists and mount rules.

Key mechanics:
- **Isolation:** Sparks run inside Docker images defined in config. Mounts are explicit and can be read-only.
- **Tool allowlists:** Each spark is configured with an allowlist of tools. The manager enforces these per step.
- **Strategy phases:** The coding strategy uses EXPLORE → EXECUTE → VERIFY phases with different tool sets and step limits.
- **Resource limits:** Docker memory/CPU/time limits are enforced per run.

Spark configuration constraints:
- `strategy` must be exactly `"code"` or `"react"`. Any other value silently fails at dispatch — no compile error or panic.
- `soul_file` is an optional path to a persona file injected into the spark's system prompt. Keep it under ~2K tokens; it counts against the completion budget.
- Default completion limit: **4096 tokens**. Default context window: **128K tokens**. Long soul files, large memory payloads, and verbose tool outputs all reduce usable context for the task.
- The local embedding model for memory search lives at `~/.athena/models/all-MiniLM-L6-v2`. If the directory is missing, memory and semantic search fail silently — run `athena doctor` to verify.

Relevant code: `src/docker.rs`, `src/manager.rs`, `src/strategy/code.rs`, `config.example.toml`.

## Model Configuration

Each provider has **two independent model slots**:

| Slot | Config key | Used by | Typical choice |
| --- | --- | --- | --- |
| Main | `model` | Spark task execution | capable/expensive model (e.g. `claude-opus-4-6`, `gpt-5.3-codex`) |
| Classifier | `classifier_model` | Orchestrator routing only (SIMPLE / DIRECT / COMPLEX) | lighter/cheaper model (e.g. `claude-haiku-4-5-20251001`, `qwen2.5:3b`) |

**Always set `classifier_model`** to a lighter model. If omitted, the main model handles routing on every request — slow and expensive.

Auth requirements differ per provider:
- **`openai`** — reads `~/.athena/openai.json` (not an env var). Default URL: `https://chatgpt.com/backend-api/codex`. Legacy `~/.athena/ouath.json` is auto-detected and migrated. (`ouath` alias is still accepted for backward compatibility.)
- **`openrouter`** — `OPENROUTER_API_KEY` env var.
- **`zen`** — `OPENCODE_API_KEY` env var.
- **`ollama`** — no auth; the Ollama daemon must be running at the configured URL (default `http://localhost:11434`).

Provider fallback order when the primary is unavailable:
```
configured provider → openai → ollama → openrouter → zen
```

Observability via Langfuse is fully opt-in. Set `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY`, and `LANGFUSE_BASE_URL` to enable tracing. No errors are emitted if these are absent — tracing is simply skipped.

Relevant code: `src/config.rs`, `src/llm.rs`.

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
    -> Execute (spark strategy)
      -> Outcome (DB + memories)
        -> Sync (write-back to ticket source)
```

Relevant code: `src/ticket_intake/`, `src/core.rs`, `src/kpi.rs`.

## Contributing as an Agent
Before opening a PR or submitting a patch:
1. Run `cargo check` (or `cargo check --all-features` if touching optional features).
2. Run `cargo test` for relevant areas.
3. Run `athena doctor --ci` for safety and system checks.
4. Run `scripts/dead_code_check.py --telegram` to verify zero dead code in both feature profiles.
5. Run `scripts/wiring_check.py` to verify all declared variants/implementations are connected.
6. Run `scripts/hygiene_check.py` to catch common AI code-smell patterns (panics, debug output, suppressors).
7. Run `scripts/maintainability_check.py` if you changed core architecture or tool behavior.
8. After each bigger change, run the relevant tests and report results without using the phrase "Tests not run." If tests fail, fix the issues and iterate until they pass.
9. If there is a suggestion to run a command, run it.
10. **Update the wiki** when relevant. Clone `https://github.com/emberloom/sparks.wiki.git`, edit the appropriate page, commit, and push. Update the wiki when you: change CLI flags or commands, add/remove config keys, change LLM provider auth, change the security model or autonomy ladder, add/remove observability event types, change memory system behavior, fix a non-obvious bug others are likely to hit (add to Troubleshooting), or change the architecture in a meaningful way. The wiki is the primary reference for users — keep it accurate.

Review expectations:
- No hardcoded credentials or tokens.
- No unsafe shell execution patterns.
- New traits should have safe defaults to avoid breaking downstream implementations.
- Database migrations are append-only and must be forward compatible.
- Keep risk tier defaults conservative.

## CodeQL Security Rules (enforced by CI — `cargo-audit` + CodeQL workflow)

These patterns trigger GitHub code-scanning alerts. Avoid them:

### No hard-coded cryptographic values — even in tests
Do **not** use string or byte literals as HMAC/crypto keys in test code:
```rust
// BAD — triggers rust/hard-coded-cryptographic-value
let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();

// GOOD — generate a random key at test time
use rand::RngCore;
let mut key = [0u8; 32];
rand::thread_rng().fill_bytes(&mut key);
let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
```

### Always use HTTPS for external API calls that carry credentials
When constructing HTTP clients that send API keys, tokens, or Basic Auth credentials, validate that the target URL is HTTPS. Warn (or reject) if the URL uses plain HTTP:
```rust
// GOOD — guard against cleartext credential transmission
if !base_url.starts_with("https://") {
    tracing::warn!(url = %base_url, "URL is not HTTPS; credentials will be sent in cleartext");
}
```
This applies to any client that calls `.basic_auth()`, `.bearer_auth()`, or sets `Authorization` headers.

### Do not log credential-adjacent values
Avoid passing values derived from auth tokens, account IDs, or API keys into `tracing::info!`, `tracing::debug!`, `eprintln!`, or `println!` in library/daemon code. CLI status output that the user explicitly requested (e.g. `athena openai status`) is the exception; document it with a comment.

### Dismissing false-positive CodeQL alerts
If CodeQL flags a false positive (e.g. provider names mistaken for credentials due to taint-flow through LLM provider objects), dismiss it via the GitHub API with `dismissed_reason=false positive` and a clear comment explaining why it is not a real issue. Do **not** add `#[allow(...)]` suppressors or restructure production code solely to silence the scanner.

## Wiring Invariants
When extending the system, these invariants must hold (enforced by `scripts/wiring_check.py` on every CI run):

- **New `ObserverCategory` variant** → add at least one `observer.log(ObserverCategory::X, …)` call at the relevant emit site.
- **New `PulseSource` variant** → use it in at least one `Pulse::new(PulseSource::X, …)` call. Mark with `#[cfg(test)]` if the variant is test-only.
- **New `LlmProvider` impl** → add a match arm in `config.rs::build_llm_provider_for()`.
- **New ticket intake provider module** → register it in the dispatch block in `core.rs`.

Violating any of these will fail the "Wiring & plumbing check" CI step. See `docs/architecture.md` for the full component map and state machines.

The "Hygiene check" CI step additionally enforces:
- No `todo!()` / `unimplemented!()` outside test blocks
- No `dbg!()` anywhere
- No `#![allow(...)]` crate-level suppressors
- No `unsafe` blocks without a preceding `// SAFETY:` comment
- No `use ...::*` glob imports outside test blocks (add `// hygiene: allow` for documented crate conventions)
- `.unwrap()`, `.expect()`, `#[allow(clippy::too_many_arguments)]`, and `#[allow(dead_code)]` counts must not regress beyond the stored baseline in `docs/hygiene-baseline.json`.

## Risks and Mitigations
| Threat | Mitigation |
| --- | --- |
| Credential exfiltration | No inline secrets; keyring + env vars; path validation in tools. |
| Prompt injection | Tool allowlists, explicit confirmation flows, constrained strategies. |
| Privilege escalation | Docker isolation, limited mounts, sensitive command filters. |
| Data loss | No destructive git operations by default; guarded shell patterns. |

## Self-Improvement Loop
Emberloom continuously scans for health signals, memory gaps, and maintainability issues. Background loops collect tool usage stats, store health alerts/fixes, and periodically re-index code for refactoring opportunities.

Relevant code: `src/proactive/`, `src/self_heal.rs`, `src/kpi.rs`.

## Key References
- Core orchestration: `src/core.rs`
- Sparks and strategy phases: `src/strategy/`
- Tool sandboxing and validation: `src/tools.rs`
- Ticket intake and sync: `src/ticket_intake/`
- Configuration and security: `src/config.rs`, `config.example.toml`
- Diagnostics: `src/doctor.rs`
- Architecture diagrams and wiring map: `docs/architecture.md`
