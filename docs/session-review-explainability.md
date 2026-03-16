# Session Review & Explainability

Emberloom records chat/tool/task activity and exposes review and explainability workflows in Telegram.

## Scope

This feature is available in Telegram flows (`feature = telegram`) and is backed by SQLite activity logs.

Core storage:
- `session_activity_log`
- `review_alert_rules`

## Telegram Commands

- `/review [summary|standard|detailed] [hours]`
- `/explain [summary|standard|detailed] [hours]`
- `/watch [seconds]`
- `/search <query>`
- `/alerts`
- `/alerts add <name> <pattern> [target] [severity]`
- `/alerts remove <id>`
- `/alerts toggle <id>`

Defaults and limits:
- `review/explain` default to `standard 24`.
- `watch` defaults to `300` seconds and is capped at `3600`.
- `search` returns recent matches across sessions.

## What Is Logged

Activity entries capture:
- event type (`chat_in`, `chat_out`, `tool_run`, `task_start`, `task_finish`, `task_fail`)
- summary/detail text
- tool name/input/output (where available)
- spark, task id, duration, parent linkage

Review/explain commands merge:
- current Telegram session activity
- autonomous activity (`session_key = "autonomous"`)

## Alert Rules

Alert rules are pattern-based and can target:
- `tool_name`
- `summary`
- `detail`
- `tool_input`
- `tool_output`
- `spark`
- `event_type`
- `any`

Severities:
- `info`
- `warn`
- `critical`

## Explainability Flow

`/explain` builds a conceptual narrative from filtered activity entries and asks the configured LLM to summarize:
- goals and outcomes
- key tool decisions
- notable failures/retries
- spark strategy choices
- follow-up risks/actions

## Operator Notes

- Use `/review detailed 24` for audit/debug sessions.
- Use `/watch 600` during long-running autonomous activity.
- Use `/alerts add` for proactive notification on risky tool patterns.
