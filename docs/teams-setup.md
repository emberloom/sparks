# Microsoft Teams Integration Setup

Guide for connecting Sparks to Microsoft Teams via the `--features teams` build flag.

## Prerequisites

- A Microsoft 365 tenant where you have app registration permissions
- Azure portal access to register a Bot Framework app
- Rust toolchain (Sparks builds with `cargo`)
- A running Sparks instance (database, LLM provider configured)
- A public HTTPS endpoint (or ngrok for local development)

## 1. Register an Azure Bot

### Create an Azure AD App Registration

1. Go to the [Azure Portal](https://portal.azure.com) and navigate to **Azure Active Directory > App registrations**
2. Click **New registration**
3. Name it (e.g. "Sparks"), select **Accounts in any organizational directory** (or your specific tenant)
4. Click **Register**
5. Note the **Application (client) ID** — this is your `app_id`

### Create a Client Secret

1. In your app registration, go to **Certificates & secrets > New client secret**
2. Add a description and expiry, then click **Add**
3. Copy the secret **value** immediately — this is your `app_password`

### Register a Bot in Azure Bot Service

1. In the Azure Portal, search for **Azure Bot** and click **Create**
2. Fill in the bot handle and select your existing app registration
3. Under **Configuration**, set the **Messaging endpoint** to:
   ```
   https://your-domain.com/api/messages
   ```
4. Note: For local development, use [ngrok](https://ngrok.com):
   ```bash
   ngrok http 3979
   # Use the https://... URL as your messaging endpoint
   ```

## 2. Install the Bot in Teams

1. In the Azure Bot resource, go to **Channels > Microsoft Teams**
2. Click **Apply** to enable the Teams channel
3. To install in your tenant, go to **Teams admin center > Manage apps** or sideload the app manifest

### App Manifest (sideload)

Create a `manifest.json`:

```json
{
  "$schema": "https://developer.microsoft.com/en-us/json-schemas/teams/v1.14/MicrosoftTeams.schema.json",
  "manifestVersion": "1.14",
  "version": "1.0.0",
  "id": "<your-app-id>",
  "packageName": "com.sparks.bot",
  "developer": {
    "name": "Your Org",
    "websiteUrl": "https://your-domain.com",
    "privacyUrl": "https://your-domain.com/privacy",
    "termsOfUseUrl": "https://your-domain.com/terms"
  },
  "name": { "short": "Sparks", "full": "Sparks AI Agent" },
  "description": {
    "short": "Autonomous AI agent",
    "full": "Sparks autonomous multi-agent system for Teams"
  },
  "icons": { "outline": "outline.png", "color": "color.png" },
  "accentColor": "#FFFFFF",
  "bots": [{
    "botId": "<your-app-id>",
    "scopes": ["personal", "team", "groupChat"],
    "supportsFiles": false,
    "isNotificationOnly": false
  }],
  "permissions": ["identity", "messageTeamMembers"],
  "validDomains": ["your-domain.com"]
}
```

Zip `manifest.json` + icon files and upload via **Teams > Apps > Upload a custom app**.

## 3. Build and Configure Sparks

### Build with Teams support

```bash
cargo build --release --features teams
```

### Environment Variables

```bash
export SPARKS_TEAMS_APP_ID="your-azure-app-id"
export SPARKS_TEAMS_APP_PASSWORD="your-client-secret"
```

### config.toml

```toml
[teams]
# bind_addr = "0.0.0.0:3979"         # default
allowed_tenants = ["your-tenant-id"]  # find in Azure AD > Properties > Tenant ID
# allow_all_tenants = false           # set true only for public bots
provider = "openai"                   # LLM provider to use
confirm_timeout_secs = 300
planning_enabled = true
planning_auto = true
```

### Run

```bash
./sparks teams
```

## 4. Security Considerations

### Tenant Authorization

**Always** configure `allowed_tenants` in production:

```toml
[teams]
allowed_tenants = ["xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"]
```

Setting `allow_all_tenants = true` allows any Microsoft 365 tenant to interact with your bot. Only use this for public bots with additional authentication.

### JWT Validation

The bot performs full RS256 JWT signature verification on incoming Bot Framework tokens:

1. Extracts the `kid` from the JWT header
2. Fetches the JWKS from the Microsoft Bot Framework OpenID configuration endpoint (`https://login.botframework.com/v1/.well-known/openidconfiguration`)
3. Verifies the RS256 signature using the matching public key
4. Validates audience (must match your `app_id`), issuer (Bot Framework or Azure STS), and expiry

For production, ensure your endpoint is HTTPS-only. Never set `skip_auth = true` in production — it disables all JWT authentication.

### Client Secret Rotation

Rotate your `app_password` regularly. Azure client secrets expire (max 24 months). Update `SPARKS_TEAMS_APP_PASSWORD` before expiry to avoid downtime.

## 5. Commands

| Command | Description |
|---------|-------------|
| `help` | Show available commands |
| `status` | Show system status and uptime |
| `run <task>` | Dispatch a task to the AI agent |
| `plan` | Start an interactive planning interview |
| `memory <query>` | Search long-term memory |
| `review [detail] [hours]` | Review recent session activity |
| `explain [detail] [hours]` | AI-powered explanation of recent activity |
| `search <query>` | Search session history |
| `alerts [list|add|remove|toggle]` | Manage alert rules |
| `health` | Run connectivity diagnostics |

You can also @mention the bot with any message to start a chat session.

### Planning Interview

The planning interview uses Adaptive Cards (interactive buttons and forms). Type `plan` to start:

1. **Goal** — Describe what you want to plan
2. **Timeline & Scope** — Select timeline and scope via buttons
3. **Output & Depth** — Choose output format and analysis depth
4. Sparks generates a full plan based on your inputs

### Confirmation Dialogs

When Sparks needs to execute a potentially destructive tool, it sends an Adaptive Card with **Approve** / **Deny** buttons. Confirmations time out after `confirm_timeout_secs` (default: 5 minutes).

## 6. Monitoring

### Health Endpoint

The bot exposes a health endpoint at `/api/health`:

```bash
curl http://localhost:3979/api/health
# → ok
```

Use this for load balancer health checks.

### Logs

Enable debug logging for Teams activity:

```bash
RUST_LOG=sparks=debug ./sparks teams
```

Key log fields:
- `activity_type` — message, invoke, conversationUpdate
- `error` — authentication or delivery failures

## 7. Troubleshooting

| Problem | Likely Cause | Fix |
|---------|--------------|-----|
| `401 Unauthorized` | Invalid JWT | Check `app_id` matches Azure registration |
| Bot not responding | Wrong messaging endpoint | Verify endpoint URL in Azure Bot config |
| `No allowed_tenants` error | Missing config | Add `allowed_tenants` or set `allow_all_tenants = true` |
| `No access_token` in logs | Wrong `app_password` | Regenerate client secret in Azure |
| Cards not rendering | Old Teams client | Update Teams or check Adaptive Card schema version |
