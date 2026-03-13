# OpenAI-Compatible API

Emberloom can expose a minimal OpenAI-compatible surface for IDE/client integrations.

## Endpoints

- `GET /v1/models`
- `POST /v1/chat/completions`

## Configuration

Configure in `config.toml`:

```toml
[openai_api]
enabled = true
bind = "127.0.0.1:8787"
api_key_env = "ATHENA_OPENAI_API_KEY"
principal = "self"
requests_per_minute = 120
burst = 30
# advertised_models = ["athena", "athena/coder", "athena/scout"]
```

Set the bearer token:

```bash
export ATHENA_OPENAI_API_KEY="change-me"
```

## Supported Request Fields (`/v1/chat/completions`)

- `model` (required)
- `messages` (required)
- `user` (optional)
- `temperature` (optional finite number)
- `stream` (optional; only `false`/unset supported)

## Known Deviations

- `stream=true` is rejected with `400`.
- Function/tool calling fields are rejected with `400`:
  - `tools`
  - `functions`
  - `tool_choice`
  - `function_call`
  - `response_format`
- Invalid/missing bearer token returns `401`.
- Core timeout returns `504`.
- Usage token counts in responses are currently placeholder values (`0`).

## Example

```bash
curl -sS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $ATHENA_OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"athena",
    "messages":[{"role":"user","content":"Summarize latest KPI trends"}],
    "user":"ide-local"
  }'
```
