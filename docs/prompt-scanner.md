# Prompt Scanner

Emberloom applies an input-layer prompt scanner before LLM/classifier execution for chat and autonomous task intake.

## Enforcement Points

- chat requests (`ChatIn`/`ChatOut` path)
- autonomous tasks before dispatch execution

Blocked input is rejected before model execution.

## Modes

- `flag_only` (default): findings are logged, request continues
- `block`: high-risk inputs are rejected

Decision states:
- `allow`
- `flag`
- `block`

## Configuration

```toml
[prompt_scanner]
enabled = true
mode = "flag_only"
flag_threshold = 4
block_threshold = 8
max_findings = 8
max_log_chars = 180

[prompt_scanner.allowlist]
ticket_ids = []
repos = []
providers = []
authors = []
text_patterns = []
downgrade_block_to_flag = true

# [[prompt_scanner.mode_overrides]]
# provider = "linear"
# repo = "LNY"
# mode = "block"
```

## Findings

Scanner rules cover common risk classes, including:
- override attempts
- secret exfiltration patterns
- shell-injection-like patterns
- tool escalation requests
- policy bypass attempts

Final score is severity-weighted and compared against thresholds.

## Allowlist and Overrides

Allowlist can match by:
- ticket id
- repo
- provider
- author
- regex text patterns

When `downgrade_block_to_flag = true`, allowlisted blocked inputs are downgraded to flagged inputs.

`mode_overrides` can raise/lower enforcement per provider/repo.

## Observability

Scanner decisions are emitted to observer logs with:
- decision
- score
- mode used
- allowlist state
- matched rule ids
- provider/repo metadata
- redacted input excerpt
