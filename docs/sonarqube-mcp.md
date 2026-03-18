# SonarQube MCP Quality Gate Integration

This document describes the 3-phase integration of SonarQube into Sparks.

---

## Overview

SonarQube is integrated as a mandatory quality gate in the Sparks CI pipeline. Before any
PR is opened, the agent verifies that the analysis is passing. The integration is designed
so that Phase 1 requires zero code changes — only MCP configuration.

---

## Phase 1 — MCP Container (zero code change)

Add the `sonarqube-mcp` Docker container to Sparks' MCP server list. Once added, agents
can call SonarQube analysis tools directly via the MCP protocol for on-demand quality
checks during development.

### MCP config snippet (`~/.sparks/config.toml`)

```toml
[mcp]
enabled = true

[[mcp.servers]]
name = "sonarqube"
command = "docker"
args = [
  "run", "--rm", "-i",
  "-e", "SONAR_TOKEN",
  "-e", "SONAR_HOST_URL",
  "ghcr.io/sapientpants/sonarqube-mcp-server:latest"
]
env = { SONAR_TOKEN = "${SPARKS_SONAR_TOKEN}", SONAR_HOST_URL = "https://sonarcloud.io" }
```

### Required environment variables for Phase 1

| Variable            | Description                                     |
|---------------------|-------------------------------------------------|
| `SPARKS_SONAR_TOKEN`| SonarQube / SonarCloud authentication token    |
| `SONAR_HOST_URL`    | Server URL (defaults to https://sonarcloud.io) |

Once configured, agents can call tools such as `sonarqube_get_quality_gate_status`,
`sonarqube_get_metrics`, and `sonarqube_get_issues` via the MCP interface.

---

## Phase 2 — CI Quality Gate (this PR)

Phase 2 wires the quality gate into the Sparks CI pipeline as a mandatory step before PR
creation. Two integration points are provided:

### 2a. Rust client (`src/sonarqube.rs`)

`SonarClient::wait_for_gate()` polls until the quality gate passes, times out, or fails
definitively. It is driven by `SonarqubeConfig` from `config.toml`.

```rust
use sparks::sonarqube::SonarClient;
use sparks::config::SonarqubeConfig;

let client = SonarClient::new(config.sonarqube.clone());
let result = client.wait_for_gate().await?;
if !result.is_passing() {
    eprintln!("{}", result.summary());
    // block PR creation
}
```

### 2b. Python CLI poller (`scripts/sonarqube_gate.py`)

For use in shell-based CI pipelines, pre-push hooks, and GitHub Actions:

```bash
python3 scripts/sonarqube_gate.py \
  --project-key myorg_myproject \
  --timeout 120 \
  --poll 5
# exits 0 on pass, 1 on failure or timeout
```

Environment-variable driven (no flags needed if env vars are set):

```bash
export SONAR_PROJECT_KEY=myorg_myproject
export SPARKS_SONAR_TOKEN=squ_...
python3 scripts/sonarqube_gate.py
```

### GitHub Actions example

```yaml
- name: Wait for SonarQube quality gate
  env:
    SONAR_TOKEN: ${{ secrets.SONAR_TOKEN }}
    SONAR_PROJECT_KEY: myorg_myproject
    SONAR_ORGANIZATION: myorg
  run: python3 scripts/sonarqube_gate.py --timeout 180
```

---

## Phase 3 — Cartograph Quality Signals (future)

Phase 3 feeds historical SonarQube metrics into Cartograph's Dynamics layer as quality
signals. This enables the agent to:

- Track quality trends over time alongside commit metadata
- Use declining code coverage or rising technical debt as signals for proactive refactoring
- Correlate quality regressions with specific contributors or module changes

Implementation will extend `src/sonarqube.rs` with a metric history collector and expose
the data through a Cartograph-compatible signal feed.

---

## Configuration Reference

All fields live under `[sonarqube]` in `config.toml`.

| Field                | Type     | Default                    | Description                                              |
|----------------------|----------|----------------------------|----------------------------------------------------------|
| `server_url`         | string   | `https://sonarcloud.io`    | SonarQube server URL                                     |
| `token`              | string?  | —                          | Auth token (prefer `SPARKS_SONAR_TOKEN` env var)         |
| `project_key`        | string?  | —                          | Project key, e.g. `myorg_myproject`                      |
| `organization`       | string?  | —                          | Organisation key (SonarCloud only)                       |
| `gate_timeout_secs`  | integer  | `120`                      | Max seconds to wait for quality gate                     |
| `poll_interval_secs` | integer  | `5`                        | Seconds between API polls                                |
| `block_on_failure`   | bool     | `true`                     | Block PR creation when gate fails                        |

### Environment variables

| Variable              | Description                                         |
|-----------------------|-----------------------------------------------------|
| `SPARKS_SONAR_TOKEN`  | Auth token (overrides `token` field in config)      |
| `SONAR_TOKEN`         | Alias accepted by the Python CLI poller             |
| `SONAR_HOST_URL`      | Server URL for CLI poller                           |
| `SONAR_PROJECT_KEY`   | Project key for CLI poller                          |
| `SONAR_ORGANIZATION`  | Organisation for CLI poller                         |

---

## Typical workflow

```
write code
    |
    v
sparks commit (or push trigger)
    |
    v
SonarQube analysis runs (scanner in CI)
    |
    v
scripts/sonarqube_gate.py  (or SonarClient::wait_for_gate())
    |
    +-- PASS --> open PR
    |
    +-- FAIL --> report failed conditions --> fix --> repeat
```

---

## Self-hosted SonarQube

For a self-hosted instance, change `server_url` and omit `organization`:

```toml
[sonarqube]
server_url = "http://localhost:9000"
project_key = "myproject"
# token set via SPARKS_SONAR_TOKEN env var
gate_timeout_secs = 180
poll_interval_secs = 10
block_on_failure = true
```
