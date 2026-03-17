# Proactive Alerting Engine

Sparks includes a background alerting engine that periodically evaluates alert rules against the activity log and delivers notifications when patterns are matched.

## Enabling

The alerting engine is enabled by default. Add or adjust the `[alerts]` section in your `config.toml`:

```toml
[alerts]
enabled = true
check_interval_secs = 30
delivery_channel = "log"
min_severity = "info"
silence_secs = 300
```

Set `enabled = false` to disable the engine entirely.

## Configuration Reference

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `true` | Enable the alerting engine |
| `check_interval_secs` | `30` | How often rules are evaluated (seconds) |
| `delivery_channel` | `"log"` | Where to send alerts: `"log"`, `"webhook"`, `"slack"`, `"teams"` |
| `webhook_url` | — | Required when `delivery_channel = "webhook"` |
| `min_severity` | `"info"` | Minimum severity to deliver: `"info"`, `"warning"`, `"critical"` |
| `silence_secs` | `300` | Suppress repeat alerts per rule for this many seconds (5 min) |

## Delivery Channels

### log (default)

Alerts are written to the structured tracing log at the appropriate level:

- `critical` -> `tracing::error!`
- `warning` -> `tracing::warn!`
- `info` -> `tracing::info!`

### webhook

Sends a JSON POST to the configured URL. Compatible with PagerDuty event endpoints, Slack incoming webhooks, and custom integrations.

```toml
[alerts]
delivery_channel = "webhook"
webhook_url = "https://hooks.example.com/your-token"
```

**Payload format:**

```json
{
  "alert": "sensitive-file-access",
  "severity": "critical",
  "pattern": ".env",
  "matched": "Read file .env",
  "fired_at": "2025-01-01T10:00:00Z",
  "message": "Alert: sensitive-file-access — matched \"Read file .env\" in activity log"
}
```

### slack / teams

When running in a Slack or Teams bot context, alerts are logged via `tracing::info!` and can be forwarded to the appropriate delivery bus. Full native bot delivery support will be added in a future iteration.

## Managing Alert Rules

Alert rules are managed via the Telegram bot interface (requires `--features telegram`):

```
/alerts                              — list all rules
/alerts add <name> <pattern> [target] [severity]
/alerts remove <id>
/alerts toggle <id>
```

**Examples:**

```
/alerts add sensitive-files .env tool_input critical
/alerts add task-failures task_fail event_type warning
/alerts add secret-access SECRET any info
```

**Targets** control which field is matched:

| Target | Description |
|--------|-------------|
| `tool_name` | Name of the tool executed |
| `summary` | Event summary text |
| `detail` | Detailed event description |
| `tool_input` | Tool input argument |
| `tool_output` | Tool output text |
| `ghost` | Ghost name that ran the event |
| `event_type` | Event type label |
| `any` (default) | Matches against all fields |

**Severities:** `info`, `warning`, `critical`

## Silence Window

The silence window prevents alert spam. Once a rule fires, it will not fire again for `silence_secs` (default 300 = 5 minutes), even if new matching activity occurs.

To make a rule fire on every check interval, set `silence_secs = 0`.

## How It Works

1. The engine starts in the background when the Telegram bot is launched.
2. Every `check_interval_secs`, it fetches all enabled alert rules from the database.
3. For each rule, it searches the activity log for recent entries matching the rule's pattern.
4. Entries older than `check_interval_secs * 2` are considered stale and skipped.
5. Rules below `min_severity` are skipped.
6. Rules within their silence window are skipped.
7. Matching rules fire a `FiredAlert` which is delivered to the configured channel.
