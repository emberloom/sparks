# Security Policy

## Supported Version

Security fixes are provided for the current `main` branch and latest tagged release.

## Reporting a Vulnerability

Please do not open public issues for suspected vulnerabilities.

Use one of these channels:

1. Open a private GitHub Security Advisory draft for this repository.
2. If that is unavailable, open a minimal issue asking maintainers for a private contact path (without exploit details).

Include:

- affected commit/tag
- impact summary
- reproduction steps
- suggested mitigation (if known)

## Secret Handling

- keep credentials out of `config.toml`
- prefer environment variables or a gitignored `.env` file

## Local-Only Operation

Emberloom supports an explicit local runtime profile:

- set `[runtime].profile = "local_only"`
- set `[llm].provider = "ollama"` with loopback URL (`localhost`, `127.0.0.1`, or `::1`)
- disable outbound integrations (`langfuse`, ticket intake sources/webhook)

Use `athena doctor` to verify readiness and detect drift:

- `athena doctor --skip-llm --ci --fail-on-warn` for config/invariant checks
- `athena doctor --ci --fail-on-warn` for live local Ollama reachability

Reference guide: `docs/local-only-deployment.md`

## Scope and Limits

- `local_only` constrains Emberloom runtime configuration and integrations.
- Spark execution is container-hardened by default (`network_mode=none`, dropped capabilities, read-only rootfs).
- Host-level outbound controls (firewall/egress policy) remain operator responsibility.
