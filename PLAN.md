# Plan: Slack Integration for Athena

## Overview

Add Slack as a messaging platform frontend, following the same pattern as the existing Telegram integration. Slack will be a new optional Cargo feature (`slack`) that allows Athena to run as a Slack bot, receiving messages via Slack's Socket Mode (no public URL needed) or Events API, and responding in channels/DMs.

## Architecture

The existing architecture is well-suited for adding new frontends:

- **`CoreHandle`** is the platform-agnostic interface — frontends call `handle.chat(session, input, confirmer)` and receive `CoreEvent` streams
- **`SessionContext`** identifies platform/user/chat — Slack would use `platform: "slack"`
- **`Confirmer` trait** is frontend-agnostic — Slack needs its own implementation (interactive buttons)
- **Feature flags** gate platform code — Telegram uses `#[cfg(feature = "telegram")]`

## Implementation Steps

### Step 1: Add Cargo feature and dependency

**File: `Cargo.toml`**

```toml
[features]
slack = ["slack-morphism"]

[dependencies]
slack-morphism = { version = "2", features = ["hyper", "axum"], optional = true }
```

[`slack-morphism`](https://crates.io/crates/slack-morphism) is a well-maintained, pure-Rust Slack API client supporting:
- Socket Mode (WebSocket-based, no public URL — ideal for self-hosted)
- Events API (webhook-based, for production deployments)
- Interactive messages (buttons for confirmations)
- Block Kit message formatting

### Step 2: Add configuration

**File: `src/config.rs`** — Add `SlackConfig` struct:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SlackConfig {
    /// Bot token (xoxb-...) — prefer ATHENA_SLACK_BOT_TOKEN env var
    pub bot_token: Option<String>,
    /// App-level token (xapp-...) for Socket Mode — prefer ATHENA_SLACK_APP_TOKEN env var
    pub app_token: Option<String>,
    /// Optional LLM provider override (default: inherits from top-level)
    pub provider: Option<String>,
    /// Allowed channel IDs (empty = deny all unless allow_all = true)
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    /// Allow all channels (must be explicitly true)
    #[serde(default)]
    pub allow_all: bool,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    /// Enable planning interview flow
    #[serde(default = "default_planning_enabled")]
    pub planning_enabled: bool,
    /// Auto-start planning for planning-like messages
    #[serde(default = "default_planning_auto")]
    pub planning_auto: bool,
    /// Planning interview timeout in seconds
    #[serde(default = "default_planning_timeout")]
    pub planning_timeout_secs: u64,
}
```

**File: `config.example.toml`** — Add commented-out `[slack]` section:

```toml
[slack]
# bot_token = "xoxb-..."  # discouraged; use ATHENA_SLACK_BOT_TOKEN env var
# app_token = "xapp-..."  # discouraged; use ATHENA_SLACK_APP_TOKEN env var
# provider = "openai"
allowed_channels = []
allow_all = false
confirm_timeout_secs = 300
planning_enabled = true
planning_auto = true
planning_timeout_secs = 900
```

### Step 3: Create `src/slack.rs` — the Slack frontend module

This is the main implementation file (~800-1200 lines estimated), structured as:

#### 3a: Core types and state

```rust
pub struct SystemInfo { ... }  // same as Telegram's

struct SlackState {
    handle: CoreHandle,
    config: SlackConfig,
    system_info: SystemInfo,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    planning: Arc<Mutex<HashMap<String, PlanningInterview>>>,
    implementing: Arc<Mutex<HashMap<String, ImplementContext>>>,
}
```

#### 3b: Confirmer implementation

```rust
struct SlackConfirmer { ... }

#[async_trait]
impl Confirmer for SlackConfirmer {
    async fn confirm(&self, action: &str) -> Result<bool> {
        // Send interactive message with Approve/Deny buttons
        // Wait on oneshot receiver with timeout
    }
}
```

#### 3c: Message formatting helpers

- Convert Markdown to Slack Block Kit / mrkdwn format
- Handle message chunking (Slack limit: 4000 chars per message, 50 blocks per message)
- Status/energy bar formatting (reuse patterns from Telegram)

#### 3d: Event handlers

- **Message events**: Handle `message` and `app_mention` events
  - Auth check (is channel allowed?)
  - Rate limiting per channel/user
  - Planning interview flow (port from Telegram)
  - Forward to `CoreHandle::chat()` and stream responses back
- **Interactive events**: Handle button clicks for confirmations
- **Slash commands** (optional): `/athena <prompt>` as an alternative to mentions

#### 3e: Response streaming

- Use Slack's `chat.postMessage` + `chat.update` to show streaming responses
- Post initial "Thinking..." message, then update it as `CoreEvent::StreamChunk` arrives
- Final update with complete response on `CoreEvent::Response`

#### 3f: Bot commands

Port key commands from Telegram:
- `/status` → Show system info, uptime, energy
- `/ghosts` → List available ghosts
- `/memory` → Memory stats
- `/help` → Command listing

In Slack these would be either slash commands or keyword-triggered (e.g., "athena status").

#### 3g: Entry point

```rust
pub async fn run_slack(
    handle: CoreHandle,
    config: SlackConfig,
    system_info: SystemInfo,
) -> anyhow::Result<()> {
    // Validate config (token present, allowed_channels or allow_all)
    // Create Slack client
    // Start Socket Mode listener or Events API server
    // Dispatch events to handlers
}
```

### Step 4: Wire into `main.rs`

Add a `Slack` CLI subcommand, gated behind `#[cfg(feature = "slack")]`:

```rust
#[cfg(feature = "slack")]
mod slack;

// In Commands enum:
/// Run as a Slack bot (requires --features slack)
#[cfg(feature = "slack")]
Slack,

// In match block:
#[cfg(feature = "slack")]
Some(Commands::Slack) => {
    let mut slack_config = config.clone();
    // ... provider override logic (same pattern as Telegram)
    let handle = AthenaCore::start(slack_config.clone(), memory).await?;
    slack::run_slack(handle, slack_config.slack, system_info).await?;
}
```

### Step 5: Update build/CI

- **`Makefile`**: Add `slack` target (like existing `telegram` target)
- **`Dockerfile`**: Add build variant with `--features slack`
- Ensure `cargo check --features slack` passes in CI

### Step 6: Documentation

- Update `config.example.toml` with Slack section (Step 2)
- Add setup instructions in `docs/` for creating a Slack app:
  1. Create app at api.slack.com
  2. Add bot scopes: `chat:write`, `app_mentions:read`, `channels:history`, `im:history`, `groups:history`
  3. Enable Socket Mode, get app-level token
  4. Install to workspace, get bot token
  5. Configure Athena with tokens

### Step 7: Tests

- Unit tests for message formatting/chunking helpers
- Unit tests for auth/channel filtering logic
- Integration test with mock Slack API (if `slack-morphism` supports it)

## Security Considerations (Public Repo)

- **No tokens/secrets in code or config** — follow existing pattern of env vars / keyring / `.env`
- **Channel allowlist by default** — same security model as Telegram's `allowed_chats`
- **`allow_all = false` by default** — bot refuses to start without explicit channel list or `allow_all = true`
- **Rate limiting** — per-channel/user rate limits to prevent abuse
- **Input sanitization** — reuse existing `prompt_scanner` for all Slack inputs
- **No internal URLs, endpoints, or org-specific config** in committed code

## Estimated Scope

| Component | Estimated Lines | Effort |
|-----------|----------------|--------|
| `Cargo.toml` changes | ~5 | Small |
| `config.rs` additions | ~50 | Small |
| `slack.rs` (new file) | ~800-1200 | Large |
| `main.rs` wiring | ~30 | Small |
| `config.example.toml` | ~15 | Small |
| Build/CI updates | ~10 | Small |
| Docs | ~100 | Medium |
| Tests | ~200 | Medium |

## Open Questions

1. **Socket Mode vs Events API**: Socket Mode is simpler (no public URL needed) but has connection limits. Events API scales better but requires a public endpoint. Recommend: **support both**, default to Socket Mode.
2. **Thread replies**: Should Athena reply in threads or in the main channel? Recommend: **reply in threads** to keep channels clean, with a config option.
3. **Slash commands**: Register `/athena` slash command in addition to `@athena` mentions? Recommend: **yes**, as it's a better UX for direct interaction.
4. **File/image support**: Telegram supports voice messages via STT. Should Slack support file uploads (code files, images)? Recommend: **phase 2** — start with text-only.
