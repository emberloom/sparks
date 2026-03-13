# Security Attestation (`sparks doctor --security`)

Sparks can emit a falsifiable security posture report for each configured ghost.

## Commands

- Text report:
  - `cargo run -- doctor --security`
- Machine-readable report:
  - `cargo run -- doctor --security --json`

`--json` is designed for CI consumers and uses schema version `v1`.

## Text Output Example

```text
Sparks Security Attestation
Schema: v1
Generated: 2026-03-03T12:34:56.789Z
Overall: PASS (ghosts=2, failing_ghosts=0, pass=20, warn=0, fail=0)

Ghost: coder [PASS]
  Effective controls: container_mode=docker caps_dropped=ALL rootfs_readonly=true network_mode=none
  Limits: pid_limit=256 memory_limit=268435456 cpu_quota=50000
  Guards: tool_allowlist=true path_validation=true sensitive_patterns=true
  [PASS] container.mode | observed=docker | expected=docker
  [PASS] container.caps_dropped | observed=ALL | expected=includes ALL
  [PASS] rootfs.readonly | observed=true | expected=true
  ...
```

## JSON Output Example (`v1`)

```json
{
  "schema_version": "v1",
  "generated_at": "2026-03-03T12:34:56.789Z",
  "summary": {
    "overall_status": "pass",
    "ghosts_total": 2,
    "ghosts_failing": 0,
    "checks_pass": 20,
    "checks_warn": 0,
    "checks_fail": 0
  },
  "ghosts": [
    {
      "name": "coder",
      "status": "pass",
      "effective_controls": {
        "container_mode": "docker",
        "caps_dropped": ["ALL"],
        "rootfs_readonly": true,
        "network_mode": "none",
        "pid_limit": 256,
        "memory_limit": 268435456,
        "cpu_quota": 50000,
        "tool_guard": true,
        "path_guard": true,
        "sensitive_pattern_guard": true
      },
      "checks": [
        {
          "id": "container.mode",
          "status": "pass",
          "observed": "docker",
          "expected": "docker",
          "remediation": null
        }
      ]
    }
  ]
}
```

## Check IDs and Meaning

- `container.mode`: Ghost execution uses Docker container mode.
- `container.caps_dropped`: Linux capabilities include `ALL` dropped.
- `rootfs.readonly`: Container root filesystem is read-only.
- `network.mode`: Container networking is disabled (`none`).
- `limits.pid`: PID limit is set and conservative (`<= 256`).
- `limits.memory`: Memory limit is set to a positive value.
- `limits.cpu`: CPU quota is set to a positive value.
- `guard.tool_allowlist`: Ghost has explicit non-empty tool allowlist.
- `guard.path_validation`: File/path tool path validation is enabled.
- `guard.sensitive_patterns`: Sensitive shell-pattern guard is configured.

Each non-pass check includes a remediation hint in `checks[].remediation`.

## CI Interpretation

- Treat any `checks[].status == "fail"` as a security regression.
- Use `checks[].id`, `observed`, `expected`, and `remediation` for actionable diagnostics.
- For schema consumers, pin `schema_version == "v1"`.
