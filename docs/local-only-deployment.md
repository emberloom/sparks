# Local-Only Deployment and Verification

This guide defines Sparks's `local_only` runtime profile for fully local operation where prompts and code stay on the machine.

## What `local_only` Means

When `[runtime].profile = "local_only"`, operator intent is:

- LLM traffic stays local (`[llm].provider = "ollama"` + loopback Ollama URL)
- external outbound integrations are disabled (`langfuse`, ticket intake sources/webhook)
- state is stored on local filesystem paths (`db.path`, `embedding.model_dir`)

`sparks doctor` includes **Funnel 5: Local-Only Deployment Readiness** to verify these invariants.

## Minimal Config

Use this as a baseline (for example `config.local-only.toml`):

```toml
[runtime]
profile = "local_only"

[llm]
provider = "ollama"

[ollama]
url = "http://127.0.0.1:11434"
model = "qwen2.5:7b"
classifier_model = "qwen2.5:3b"

[db]
path = "~/.sparks/sparks.db"

[embedding]
enabled = true
model_dir = "~/.sparks/models/all-MiniLM-L6-v2"

[ticket_intake]
enabled = false
sources = []

[ticket_intake.webhook]
enabled = false

[langfuse]
enabled = false
```

## Setup Path

1. Start Ollama locally and pull required models.
2. Use a config with `[runtime].profile = "local_only"`.
3. Run local-only doctor checks:

```bash
SPARKS_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --skip-llm --ci --fail-on-warn
```

4. Run reachability check (without `--skip-llm`) after Ollama is up:

```bash
SPARKS_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --ci --fail-on-warn
```

Expected: `Overall: PASS` and all checks in Funnel 5 are `PASS`.

## Red-Team Verification Checklist

Use these checks to detect accidental outbound behavior or profile drift.

1. Provider drift test

```bash
# Change llm.provider -> "openai" and rerun doctor
SPARKS_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --skip-llm --ci --fail-on-warn
```

Expected: fail at `Local model provider`.

2. Endpoint drift test

```bash
# Change ollama.url -> "http://example.com:11434" and rerun doctor
SPARKS_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --skip-llm --ci --fail-on-warn
```

Expected: fail at `Ollama endpoint loopback`.

3. Outbound integration drift test

```bash
# Enable [langfuse] or configure ticket_intake.sources and rerun doctor
SPARKS_DISABLE_HOME_PROFILES=1 cargo run -- --config config.local-only.toml doctor --skip-llm --ci --fail-on-warn
```

Expected: fail at `Outbound integration toggles`.

4. Egress observation (optional)

Run Sparks while observing non-loopback connections (tooling depends on host OS):

- Linux example: `sudo tcpdump -i any 'tcp and not (dst host 127.0.0.1 or dst host ::1)'`
- macOS example: `sudo tcpdump -i any 'tcp and not (dst host 127.0.0.1 or dst host ::1)'`

For local-only runs, expected outbound traffic is none.

## CI/Docs Smoke Example

See `.github/workflows/doctor.yml` for a local-only smoke + negative-drift example:

- validates a local-only config passes `sparks doctor --skip-llm --ci --fail-on-warn`
- intentionally enables `langfuse` in a copy and verifies doctor fails

## Mode Interactions

`local_only` (runtime profile), container-strict execution, and self-dev mode solve different layers and can be combined.

- `local_only`: host runtime egress policy and integration constraints.
- Container-strict execution: ghost container hardening (`network_mode=none`, dropped caps, read-only rootfs) in `src/docker.rs`.
- Trusted-host self-dev mode (`[self_dev].enabled = true`): enables autonomous diagnosis/refactoring loops in the host process.

Recommended combination for maximum locality:

1. `runtime.profile = "local_only"`
2. keep container-strict defaults enabled (default behavior)
3. disable outbound integrations (`langfuse`, ticket intake sources/webhook)
4. enable `self_dev` only if you want local autonomous loops

Precedence and interpretation:

1. `runtime.profile` defines local-only invariants checked by doctor.
2. Container-strict execution constrains in-container tool execution regardless of profile.
3. `self_dev` changes autonomy behavior; it does not relax local-only invariants.
