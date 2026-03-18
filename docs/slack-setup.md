# Slack Integration Setup

Guide for connecting Sparks to Slack via the `--features slack` build flag.

## Prerequisites

- A Slack workspace where you have admin or app-creation permissions
- Rust toolchain (Sparks builds with `cargo`)
- A running Sparks instance (database, LLM provider configured)

## 1. Create a Slack App

1. Go to [api.slack.com/apps](https://api.slack.com/apps) and click **Create New App**
2. Choose **From scratch**, name it (e.g. "Sparks"), and select your workspace

### Bot Token Scopes

Under **OAuth & Permissions > Bot Token Scopes**, add:

| Scope | Purpose |
|-------|---------|
| `app_mentions:read` | Respond to @mentions |
| `channels:history` | Read messages in public channels |
| `channels:read` | List channels for authorization |
| `chat:write` | Send messages and updates |
| `commands` | Register `/sparks` slash command |
| `files:read` | Download shared files |
| `files:write` | Upload files |
| `groups:history` | Read messages in private channels |
| `groups:read` | List private channels |
| `im:history` | Read direct messages |
| `im:read` | List DM conversations |
| `reactions:write` | Add reactions for status indicators |
| `users:read` | Resolve user display names |

### Socket Mode (Recommended)

Under **Socket Mode**, toggle it **on**. This generates an **App-Level Token** (`xapp-...`).

- No public URL required
- Works behind firewalls and NATs
- Recommended for development and most deployments

### Events API (Alternative)

If you need HTTP webhooks instead of Socket Mode:

1. Under **Event Subscriptions**, toggle **on**
2. Set the **Request URL** to `https://your-domain/slack/events`
3. Subscribe to bot events: `message.channels`, `message.groups`, `message.im`, `app_mention`
4. Note your **Signing Secret** from **Basic Information**

### Slash Command

Under **Slash Commands**, create:

| Field | Value |
|-------|-------|
| Command | `/sparks` |
| Request URL | (auto-handled in Socket Mode) |
| Short Description | Chat with Sparks |
| Usage Hint | `[help\|status\|plan\|implement\|...]` |

### Interactivity

Under **Interactivity & Shortcuts**, toggle **on**. This is required for Block Kit buttons (confirmations, planning interview, CLI picker).

In Socket Mode the request URL is handled automatically.

## 2. Install the App

1. Go to **Install App** and click **Install to Workspace**
2. Authorize the requested permissions
3. Copy the **Bot User OAuth Token** (`xoxb-...`)

## 3. Configure Sparks

### Environment Variables (Recommended)

```bash
export SPARKS_SLACK_BOT_TOKEN="xoxb-your-bot-token"
export SPARKS_SLACK_APP_TOKEN="xapp-your-app-level-token"  # Socket Mode only
export SPARKS_SLACK_SIGNING_SECRET="your-signing-secret"   # Events API only
```

### Config File

Add to your `config.toml`:

```toml
[slack]
# bot_token = "xoxb-..."        # prefer env var SPARKS_SLACK_BOT_TOKEN
# app_token = "xapp-..."        # prefer env var SPARKS_SLACK_APP_TOKEN
mode = "socket"                  # "socket" (default) or "events_api"
# events_api_bind = "127.0.0.1:3000"  # bind address for Events API mode
# provider = "openai"            # LLM provider override (optional)

# Channel access control (default: deny all)
# allowed_channels = ["C01ABCDEF", "C02GHIJKL"]
# allow_all = false              # set true to allow all channels

# Behavior
thread_replies = true            # always reply in threads (default: true)
confirm_timeout_secs = 300       # confirmation button timeout (5 minutes)

# Planning interview
planning_enabled = true          # enable /sparks plan (default: true)
planning_auto = true             # auto-detect planning requests (default: true)
planning_timeout_secs = 900      # stale interview cleanup (15 minutes)
```

### Configuration Reference

| Field | Default | Description |
|-------|---------|-------------|
| `bot_token` | — | Bot User OAuth Token (`xoxb-...`). Required. |
| `app_token` | — | App-Level Token (`xapp-...`). Required for Socket Mode. |
| `signing_secret` | — | Signing Secret. Required for Events API. |
| `mode` | `"socket"` | Connection mode: `socket` or `events_api` |
| `events_api_bind` | `"127.0.0.1:3000"` | HTTP bind address for Events API mode |
| `provider` | (from CLI) | LLM provider override |
| `allowed_channels` | `[]` | Channel IDs allowed to interact with Sparks |
| `allow_all` | `false` | Allow all channels (overrides `allowed_channels`) |
| `confirm_timeout_secs` | `300` | Seconds before confirmation buttons expire |
| `thread_replies` | `true` | Reply in threads |
| `planning_enabled` | `true` | Enable planning interview feature |
| `planning_auto` | `true` | Auto-start planning for detected planning requests |
| `planning_timeout_secs` | `900` | Stale planning interview cleanup threshold |

## 4. Build and Run

```bash
# Build with Slack feature
cargo build --release --features slack

# Run the Slack bot
./target/release/sparks slack

# Or with both Telegram and Slack
cargo build --release --features telegram,slack
```

## 5. Channel Access Control

By default, Sparks ignores all channels. You must explicitly allow channels:

**Option A — Allow specific channels:**
```toml
allowed_channels = ["C01ABCDEF", "C02GHIJKL"]
```

To find a channel ID: right-click the channel name in Slack > **View channel details** > scroll to the bottom.

**Option B — Allow all channels:**
```toml
allow_all = true
```

Use with caution in large workspaces.

## 6. Available Commands

All commands go through the `/sparks` slash command:

| Command | Description |
|---------|-------------|
| `/sparks help` | Show help |
| `/sparks status` | System status, uptime, model info |
| `/sparks plan [goal]` | Start interactive planning interview |
| `/sparks implement <goal>` | Implement with CLI tool |
| `/sparks model [name]` | Show or switch LLM model |
| `/sparks models` | List available models |
| `/sparks ghosts` | List active ghosts |
| `/sparks memories [query]` | List saved memories |
| `/sparks dispatch <ghost> <goal>` | Run autonomous task |
| `/sparks review [summary\|detailed] [hours]` | Activity review |
| `/sparks explain [summary\|detailed] [hours]` | Conceptual explanation |
| `/sparks watch [seconds]` | Real-time activity stream |
| `/sparks search <query>` | Search across sessions |
| `/sparks alerts` | Manage alert rules |
| `/sparks knobs` | Display runtime knobs |
| `/sparks mood` | Mood state with energy |
| `/sparks jobs` | List cron jobs |
| `/sparks session` | Current session info |
| `/sparks cli` | Switch CLI tool (interactive) |
| `/sparks set <key> <value>` | Modify a runtime knob |
| `/sparks cli_model [name]` | Show/switch CLI model |

You can also send regular messages in allowed channels or @mention the bot.

## 7. Features

### Thread-Based Conversations

When `thread_replies = true` (default), Sparks replies in threads. Each thread maintains its own session context, planning state, and confirmation buttons.

### Streaming Responses

Sparks posts a status message on dispatch, then updates it in-place as streaming chunks arrive (throttled to ~800ms). The final response replaces the status message.

### Confirmations

When Sparks needs approval for a tool action, it sends Block Kit buttons (Approve / Deny). Buttons expire after `confirm_timeout_secs`.

### Planning Interview

The planning interview walks through Goal > Constraints > Output Format > Summary with interactive Block Kit buttons at each step. After plan generation, buttons offer Implement / Refine / Done.

Auto-detection triggers on keywords like "plan", "roadmap", "strategy", "launch", "rollout", "gtm" when `planning_auto = true`.

### Pulse Delivery

Background pulses (heartbeat, memory insights, idle thoughts, mood shifts, autonomous task results) are delivered to allowed channels automatically.

## Troubleshooting

**"Socket Mode requires app_token"** — Set `SPARKS_SLACK_APP_TOKEN` or `[slack].app_token` in config. Generate one under Socket Mode in your app settings.

**Bot doesn't respond** — Check `allowed_channels` includes the channel ID, or set `allow_all = true`. Check logs for authorization denials.

**"Events API requires signing_secret"** — Only needed when `mode = "events_api"`. Set `SPARKS_SLACK_SIGNING_SECRET` or the config field.

**Buttons not working** — Ensure **Interactivity** is enabled in your Slack app settings. Socket Mode handles the request URL automatically.

**Rate limiting** — Sparks applies a 5-second per-channel cooldown on incoming messages. If messages are being dropped, this is intentional to prevent spam loops.
