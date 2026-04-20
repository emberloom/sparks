//! Microsoft Teams bot frontend (--features teams).
//!
//! Uses the Bot Framework REST API:
//! - Receives Activity objects via HTTP POST /api/messages
//! - Validates incoming JWT tokens from Microsoft
//! - Replies by POSTing to activity.serviceUrl
//! - Uses Adaptive Cards for interactive UI (confirmations, planning)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use jsonwebtoken::{decode_header, Algorithm, DecodingKey, Validation, decode};

use crate::config::TeamsConfig;
use crate::confirm::Confirmer;
use crate::core::{CoreEvent, CoreHandle, SessionContext};
use crate::error::{SparksError, Result};
use crate::session_review::ActivityEntry;

static RE_AT_MENTION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"<at>[^<]*</at>").expect("valid regex")
});

/// System info passed from main to the Teams bot.
#[derive(Clone)]
pub struct SystemInfo {
    pub provider: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub started_at: tokio::time::Instant,
}

// ── Planning types ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlanningStep {
    Goal,
    Constraints,
    Output,
    Summary,
    Done,
}

#[derive(Clone, Debug)]
struct PlanningInterview {
    goal: Option<String>,
    constraints: Option<String>,
    timeline: Option<String>,
    scope: Option<String>,
    output: Option<String>,
    depth: Option<String>,
    step: PlanningStep,
    last_updated: tokio::time::Instant,
}

impl PlanningInterview {
    fn new() -> Self {
        Self {
            goal: None,
            constraints: None,
            timeline: None,
            scope: None,
            output: None,
            depth: None,
            step: PlanningStep::Goal,
            last_updated: tokio::time::Instant::now(),
        }
    }
}

// ── Bot Framework Activity model ─────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChannelAccount {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConversationAccount {
    pub id: String,
    #[serde(rename = "isGroup", default)]
    pub is_group: bool,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Activity {
    #[serde(rename = "type")]
    pub activity_type: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    pub from: Option<ChannelAccount>,
    pub recipient: Option<ChannelAccount>,
    pub conversation: Option<ConversationAccount>,
    #[serde(rename = "serviceUrl", default)]
    pub service_url: String,
    #[serde(rename = "channelId", default)]
    pub channel_id: String,
    pub text: Option<String>,
    #[serde(rename = "replyToId", default)]
    pub reply_to_id: String,
    #[serde(rename = "channelData")]
    pub channel_data: Option<Value>,
    pub value: Option<Value>,
    pub attachments: Option<Vec<Value>>,
}

impl Activity {
    fn tenant_id(&self) -> Option<String> {
        self.channel_data.as_ref()?.get("tenant")?.get("id")?.as_str().map(String::from)
    }

    fn conversation_id(&self) -> &str {
        self.conversation.as_ref().map(|c| c.id.as_str()).unwrap_or("")
    }

    fn from_id(&self) -> &str {
        self.from.as_ref().map(|f| f.id.as_str()).unwrap_or("")
    }
}

// ── State ─────────────────────────────────────────────────────────────

/// Shared state for the Teams bot.
#[derive(Clone)]
struct TeamsState {
    handle: CoreHandle,
    http: reqwest::Client,
    app_id: String,
    app_password: String,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    last_request: Arc<Mutex<HashMap<String, tokio::time::Instant>>>,
    planning: Arc<Mutex<HashMap<String, PlanningInterview>>>,
    config: TeamsConfig,
    system_info: SystemInfo,
    /// Cached Bearer token (value, expiry as unix timestamp)
    bearer: Arc<Mutex<Option<(String, u64)>>>,
    /// Cached JWKS keys: map from kid -> (n, e) base64url components
    jwks_cache: Arc<Mutex<Option<HashMap<String, (String, String)>>>>,
}

// ── Auth ──────────────────────────────────────────────────────────────

/// Fetch or return a cached Bearer token for outbound replies.
async fn get_bearer(state: &TeamsState) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    {
        let cached = state.bearer.lock().await;
        if let Some((token, exp)) = cached.as_ref() {
            if *exp > now + 60 {
                return Ok(token.clone());
            }
        }
    }

    let params = [
        ("grant_type", "client_credentials"),
        ("client_id", state.app_id.as_str()),
        ("client_secret", state.app_password.as_str()),
        ("scope", "https://api.botframework.com/.default"),
    ];

    let resp = state
        .http
        .post("https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token")
        .form(&params)
        .send()
        .await?
        .json::<Value>()
        .await?;

    let token = resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response"))?
        .to_string();
    let expires_in = resp["expires_in"].as_u64().unwrap_or(3600);

    {
        let mut cached = state.bearer.lock().await;
        *cached = Some((token.clone(), now + expires_in));
    }

    Ok(token)
}

/// Fetch the Bot Framework JWKS and find the key for the given kid.
/// Returns the RSA public key components (n, e) in base64url format.
async fn fetch_jwks_key(http: &reqwest::Client, kid: &str) -> anyhow::Result<(String, String)> {
    // Step 1: Get OpenID config
    let oidc: Value = http
        .get("https://login.botframework.com/v1/.well-known/openidconfiguration")
        .send()
        .await?
        .json()
        .await?;
    let jwks_uri = oidc["jwks_uri"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No jwks_uri in OIDC config"))?;

    // Step 2: Get JWKS
    let jwks: Value = http.get(jwks_uri).send().await?.json().await?;
    let keys = jwks["keys"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No keys in JWKS"))?;

    // Step 3: Find our kid
    for key in keys {
        if key["kid"].as_str() == Some(kid) {
            let n = key["n"].as_str().ok_or_else(|| anyhow::anyhow!("Key missing n"))?.to_string();
            let e = key["e"].as_str().ok_or_else(|| anyhow::anyhow!("Key missing e"))?.to_string();
            return Ok((n, e));
        }
    }
    Err(anyhow::anyhow!("Key with kid={} not found in JWKS", kid))
}

/// Validate the Bot Framework JWT Bearer token.
/// Returns the app_id from the token's audience claim on success.
async fn validate_token(
    headers: &HeaderMap,
    app_id: &str,
    skip_auth: bool,
    http: &reqwest::Client,
) -> anyhow::Result<String> {
    if skip_auth {
        tracing::warn!("Teams JWT validation is DISABLED (skip_auth = true) — not safe for production");
        return Ok(app_id.to_string());
    }

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("Missing Authorization header"))?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or_else(|| anyhow::anyhow!("Authorization header must start with 'Bearer '"))?;

    // Decode header to get kid and algorithm
    let header = decode_header(token)
        .map_err(|e| anyhow::anyhow!("Failed to decode JWT header: {}", e))?;

    if header.alg != Algorithm::RS256 {
        return Err(anyhow::anyhow!("JWT algorithm must be RS256, got {:?}", header.alg));
    }

    let kid = header.kid.ok_or_else(|| anyhow::anyhow!("JWT header missing kid"))?;

    // Fetch the public key for this kid
    let (n, e) = fetch_jwks_key(http, &kid).await
        .map_err(|e| anyhow::anyhow!("Failed to fetch JWKS key: {}", e))?;

    let decoding_key = DecodingKey::from_rsa_components(&n, &e)
        .map_err(|e| anyhow::anyhow!("Failed to build RSA decoding key: {}", e))?;

    // Validate: RS256, audience must be app_id, validate exp
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[app_id]);
    // Bot Framework tokens can have either of these issuers
    validation.set_issuer(&[
        "https://api.botframework.com",
        "https://sts.windows.net/d6d49420-f39b-4df7-a1dc-d59a935871db/",
    ]);

    #[derive(serde::Deserialize)]
    struct Claims {
        aud: String,
    }

    decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|e| anyhow::anyhow!("JWT validation failed: {}", e))?;

    Ok(app_id.to_string())
}

// ── Authorization ─────────────────────────────────────────────────────

fn is_authorized(tenant_id: Option<&str>, config: &TeamsConfig) -> bool {
    // allowed_tenants takes precedence over allow_all_tenants for safety.
    if !config.allowed_tenants.is_empty() {
        return tenant_id
            .map(|t| config.allowed_tenants.iter().any(|a| a == t))
            .unwrap_or(false);
    }
    config.allow_all_tenants
}

fn should_rate_limit(
    last: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    cooldown: tokio::time::Duration,
) -> bool {
    last.map(|t| now.duration_since(t) < cooldown).unwrap_or(false)
}

// ── HTTP reply helpers ────────────────────────────────────────────────

/// Validate that a Bot Framework serviceUrl is safe to use for outbound calls.
/// Must be HTTPS and point to a known Microsoft/Bot Framework endpoint.
fn validate_service_url(service_url: &str) -> anyhow::Result<()> {
    let url = service_url.trim_end_matches('/');
    if url.is_empty() {
        return Err(anyhow::anyhow!("Empty serviceUrl"));
    }
    // Must be HTTPS
    if !url.starts_with("https://") {
        return Err(anyhow::anyhow!("serviceUrl must use HTTPS, got: {}", url));
    }
    // Must be a recognized Bot Framework or Teams domain
    let allowed_suffixes = [
        ".botframework.com",
        ".microsoft.com",
        ".teams.microsoft.com",
        ".skype.com",
    ];
    let host = url.strip_prefix("https://").unwrap_or(url);
    let host = host.split('/').next().unwrap_or(host);
    if !allowed_suffixes.iter().any(|suffix| host.ends_with(suffix)) {
        return Err(anyhow::anyhow!(
            "serviceUrl host '{}' is not a recognized Bot Framework endpoint",
            host
        ));
    }
    Ok(())
}

/// Post a reply activity to the Bot Framework service URL.
async fn post_reply(
    state: &TeamsState,
    activity: &Activity,
    reply_body: Value,
) -> anyhow::Result<()> {
    validate_service_url(&activity.service_url)?;
    let bearer = get_bearer(state).await?;
    let conv_id = activity.conversation_id();
    let url = format!(
        "{}v3/conversations/{}/activities",
        activity.service_url, conv_id
    );

    let resp = state
        .http
        .post(&url)
        .bearer_auth(&bearer)
        .json(&reply_body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Reply failed {}: {}", status, body));
    }

    Ok(())
}

/// Send a plain text reply.
async fn reply_text(state: &TeamsState, activity: &Activity, text: &str) {
    let body = json!({
        "type": "message",
        "from": activity.recipient,
        "conversation": activity.conversation,
        "recipient": activity.from,
        "text": text,
        "replyToId": activity.id,
    });
    if let Err(e) = post_reply(state, activity, body).await {
        tracing::error!(error = %e, "Failed to send Teams reply");
    }
}

/// Send an Adaptive Card reply.
async fn reply_card(state: &TeamsState, activity: &Activity, card: Value) {
    let body = json!({
        "type": "message",
        "from": activity.recipient,
        "conversation": activity.conversation,
        "recipient": activity.from,
        "replyToId": activity.id,
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "content": card,
        }],
    });
    if let Err(e) = post_reply(state, activity, body).await {
        tracing::error!(error = %e, "Failed to send Teams card reply");
    }
}

/// Update an existing message by activity ID.
async fn update_message(
    state: &TeamsState,
    activity: &Activity,
    msg_id: &str,
    text: &str,
) {
    if let Err(e) = validate_service_url(&activity.service_url) {
        tracing::error!(error = %e, "Refusing to update message: invalid serviceUrl");
        return;
    }
    let bearer = match get_bearer(state).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get bearer for update");
            return;
        }
    };
    let conv_id = activity.conversation_id();
    let url = format!(
        "{}v3/conversations/{}/activities/{}",
        activity.service_url, conv_id, msg_id
    );
    let body = json!({
        "type": "message",
        "from": activity.recipient,
        "conversation": activity.conversation,
        "recipient": activity.from,
        "text": text,
    });
    if let Err(e) = state.http.put(&url).bearer_auth(&bearer).json(&body).send().await {
        tracing::error!(error = %e, "Failed to update Teams message");
    }
}

/// Post a new message and return its ID (for later updates).
async fn post_message_get_id(
    state: &TeamsState,
    activity: &Activity,
    text: &str,
) -> Option<String> {
    validate_service_url(&activity.service_url).ok()?;
    let bearer = get_bearer(state).await.ok()?;
    let conv_id = activity.conversation_id();
    let url = format!(
        "{}v3/conversations/{}/activities",
        activity.service_url, conv_id
    );
    let body = json!({
        "type": "message",
        "from": activity.recipient,
        "conversation": activity.conversation,
        "recipient": activity.from,
        "text": text,
        "replyToId": activity.id,
    });
    let resp = state
        .http
        .post(&url)
        .bearer_auth(&bearer)
        .json(&body)
        .send()
        .await
        .ok()?;
    let val: Value = resp.json().await.ok()?;
    val["id"].as_str().map(String::from)
}

// ── Formatting helpers ────────────────────────────────────────────────

/// Escape characters that could interfere with Teams markdown.
fn escape_teams(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn chunk_message(text: &str, max: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + max).min(text.len());
        let mut actual_end = end;
        while actual_end > start && !text.is_char_boundary(actual_end) {
            actual_end -= 1;
        }
        if actual_end == start {
            actual_end = end.min(text.len());
        }
        chunks.push(&text[start..actual_end]);
        start = actual_end;
    }
    chunks
}

fn format_duration(started_at: tokio::time::Instant) -> String {
    let secs = started_at.elapsed().as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ── Adaptive Card builders ─────────────────────────────────────────────

/// Build a confirmation Adaptive Card with Approve/Deny buttons.
fn build_confirm_card(prompt: &str, confirm_id: &str) -> Value {
    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": [{
            "type": "TextBlock",
            "text": "Confirmation Required",
            "weight": "Bolder",
            "size": "Medium",
        }, {
            "type": "TextBlock",
            "text": prompt,
            "wrap": true,
        }],
        "actions": [{
            "type": "Action.Submit",
            "title": "Approve",
            "style": "positive",
            "data": { "action": "confirm", "confirm_id": confirm_id, "value": true },
        }, {
            "type": "Action.Submit",
            "title": "Deny",
            "style": "destructive",
            "data": { "action": "confirm", "confirm_id": confirm_id, "value": false },
        }],
    })
}

/// Build the planning goal-collection card.
fn build_planning_start_card() -> Value {
    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": [{
            "type": "TextBlock",
            "text": "Planning Assistant",
            "weight": "Bolder",
            "size": "Large",
        }, {
            "type": "TextBlock",
            "text": "What would you like to plan?",
            "wrap": true,
        }, {
            "type": "Input.Text",
            "id": "goal",
            "placeholder": "Describe your goal...",
            "isMultiline": true,
            "maxLength": 1000,
        }],
        "actions": [{
            "type": "Action.Submit",
            "title": "Next",
            "data": { "action": "planning", "step": "goal" },
        }],
    })
}

/// Build the planning constraints card.
fn build_planning_constraints_card() -> Value {
    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": [{
            "type": "TextBlock",
            "text": "Planning: Constraints & Scope",
            "weight": "Bolder",
        }, {
            "type": "TextBlock",
            "text": "Timeline",
            "weight": "Bolder",
            "size": "Small",
        }, {
            "type": "ActionSet",
            "actions": [
                { "type": "Action.Submit", "title": "Today", "data": { "action": "planning", "step": "timeline", "value": "today" } },
                { "type": "Action.Submit", "title": "This week", "data": { "action": "planning", "step": "timeline", "value": "this week" } },
                { "type": "Action.Submit", "title": "No deadline", "data": { "action": "planning", "step": "timeline", "value": "no deadline" } },
            ],
        }, {
            "type": "TextBlock",
            "text": "Scope",
            "weight": "Bolder",
            "size": "Small",
        }, {
            "type": "ActionSet",
            "actions": [
                { "type": "Action.Submit", "title": "Idea only", "data": { "action": "planning", "step": "scope", "value": "idea only" } },
                { "type": "Action.Submit", "title": "Implementation", "data": { "action": "planning", "step": "scope", "value": "implementation" } },
                { "type": "Action.Submit", "title": "Full plan", "data": { "action": "planning", "step": "scope", "value": "full plan" } },
            ],
        }, {
            "type": "TextBlock",
            "text": "Constraints (optional)",
            "weight": "Bolder",
            "size": "Small",
        }, {
            "type": "Input.Text",
            "id": "constraints",
            "placeholder": "Budget, tech stack, team size...",
            "isMultiline": false,
        }],
        "actions": [{
            "type": "Action.Submit",
            "title": "Next",
            "data": { "action": "planning", "step": "constraints_submit" },
        }],
    })
}

/// Build the planning output format card.
fn build_planning_output_card() -> Value {
    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": [{
            "type": "TextBlock",
            "text": "Planning: Output Format & Depth",
            "weight": "Bolder",
        }, {
            "type": "TextBlock",
            "text": "Output format",
            "weight": "Bolder",
            "size": "Small",
        }, {
            "type": "ActionSet",
            "actions": [
                { "type": "Action.Submit", "title": "Checklist", "data": { "action": "planning", "step": "output", "value": "checklist" } },
                { "type": "Action.Submit", "title": "Spec document", "data": { "action": "planning", "step": "output", "value": "spec" } },
                { "type": "Action.Submit", "title": "Draft", "data": { "action": "planning", "step": "output", "value": "draft" } },
            ],
        }, {
            "type": "TextBlock",
            "text": "Depth",
            "weight": "Bolder",
            "size": "Small",
        }, {
            "type": "ActionSet",
            "actions": [
                { "type": "Action.Submit", "title": "Quick", "data": { "action": "planning", "step": "depth", "value": "quick" } },
                { "type": "Action.Submit", "title": "Standard", "data": { "action": "planning", "step": "depth", "value": "standard" } },
                { "type": "Action.Submit", "title": "Deep", "data": { "action": "planning", "step": "depth", "value": "deep" } },
            ],
        }],
    })
}

fn planning_summary(interview: &PlanningInterview) -> String {
    let mut lines = vec!["**Planning Summary**".to_string()];
    if let Some(g) = &interview.goal { lines.push(format!("**Goal:** {}", g)); }
    if let Some(t) = &interview.timeline { lines.push(format!("**Timeline:** {}", t)); }
    if let Some(s) = &interview.scope { lines.push(format!("**Scope:** {}", s)); }
    if let Some(c) = &interview.constraints { lines.push(format!("**Constraints:** {}", c)); }
    if let Some(o) = &interview.output { lines.push(format!("**Output:** {}", o)); }
    if let Some(d) = &interview.depth { lines.push(format!("**Depth:** {}", d)); }
    lines.join("\n")
}

fn planning_build_prompt(interview: &PlanningInterview) -> String {
    let mut out = String::from("Create a plan with the following inputs:\n\n");
    out.push_str(&format!("Goal: {}\n", interview.goal.as_deref().unwrap_or("unspecified")));
    out.push_str(&format!("Constraints: {}\n", interview.constraints.as_deref().unwrap_or("none")));
    out.push_str(&format!("Timeline: {}\n", interview.timeline.as_deref().unwrap_or("unspecified")));
    out.push_str(&format!("Scope: {}\n", interview.scope.as_deref().unwrap_or("unspecified")));
    out.push_str(&format!("Output format: {}\n", interview.output.as_deref().unwrap_or("checklist")));
    out.push_str(&format!("Depth: {}\n", interview.depth.as_deref().unwrap_or("standard")));
    out
}

fn is_planning_like(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("plan") || lower.contains("roadmap") || lower.contains("strategy") ||
    lower.contains("how to") || lower.contains("steps to") || lower.contains("outline") ||
    lower.contains("breakdown") || lower.contains("sprint") || lower.contains("design")
}

// ── Confirmer ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct TeamsConfirmer {
    state: TeamsState,
    activity: Activity,
}

#[async_trait]
impl Confirmer for TeamsConfirmer {
    async fn confirm(&self, prompt: &str) -> Result<bool> {
        let confirm_id = uuid::Uuid::new_v4().to_string();
        let card = build_confirm_card(prompt, &confirm_id);
        reply_card(&self.state, &self.activity, card).await;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.state.pending.lock().await;
            pending.insert(confirm_id.clone(), tx);
        }

        let timeout_dur = tokio::time::Duration::from_secs(self.state.config.confirm_timeout_secs);
        match tokio::time::timeout(timeout_dur, rx).await {
            Ok(Ok(true)) => Ok(true),
            Ok(Ok(false)) => {
                self.state.pending.lock().await.remove(&confirm_id);
                Err(SparksError::Denied)
            }
            Err(_) | Ok(Err(_)) => {
                self.state.pending.lock().await.remove(&confirm_id);
                Err(SparksError::Cancelled)
            }
        }
    }
}

fn teams_confirmer(state: &TeamsState, activity: &Activity) -> Arc<dyn Confirmer> {
    Arc::new(TeamsConfirmer {
        state: state.clone(),
        activity: activity.clone(),
    })
}

// ── Core dispatch ─────────────────────────────────────────────────────

async fn dispatch_to_core(
    state: &TeamsState,
    activity: Activity,
    session_ctx: SessionContext,
    text: String,
    initial_status: &str,
    followup: Option<String>,
) {
    let confirmer = teams_confirmer(state, &activity);
    let status_id = post_message_get_id(state, &activity, initial_status).await;

    let session_key = session_ctx.session_key();
    let events = if state.handle.is_session_active(&session_key) {
        state.handle.inject(&session_key, text.clone());
        tracing::debug!(session_key = %session_key, "Mid-run message injected into active session");
        reply_text(state, &activity, "_Your message has been noted and will be picked up in the next step._").await;
        return;
    } else {
        match state.handle.chat(session_ctx, &text, confirmer).await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::error!(error = %e, "Teams core dispatch failed");
                reply_text(state, &activity, "_An internal error occurred._").await;
                return;
            }
        }
    };

    let state2 = state.clone();
    tokio::spawn(async move {
        forward_teams_events(state2, activity, status_id, events, followup).await;
    });
}

async fn forward_teams_events(
    state: TeamsState,
    activity: Activity,
    status_id: Option<String>,
    mut events: tokio::sync::mpsc::Receiver<CoreEvent>,
    followup: Option<String>,
) {
    let mut output_buf = String::new();
    let mut last_update = tokio::time::Instant::now();
    const UPDATE_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_millis(2000);
    const MAX_BUF: usize = 25_000;

    while let Some(ev) = events.recv().await {
        match ev {
            CoreEvent::StreamChunk(tok) => {
                output_buf.push_str(&tok);
                if output_buf.len() > MAX_BUF {
                    output_buf = format!("...(truncated)...\n{}", &output_buf[output_buf.len().saturating_sub(MAX_BUF)..]);
                }
                // Throttle updates to avoid Teams rate limits
                if last_update.elapsed() > UPDATE_INTERVAL {
                    if let Some(ref sid) = status_id {
                        update_message(&state, &activity, sid, &output_buf).await;
                        last_update = tokio::time::Instant::now();
                    }
                }
            }
            CoreEvent::Response(text) => {
                // Final response replaces the buffer
                if !text.is_empty() {
                    output_buf = text;
                }
            }
            CoreEvent::ToolRun { tool, result: _, success: _ } => {
                let msg = format!("Running `{}`...", tool);
                if let Some(ref sid) = status_id {
                    let display = if output_buf.is_empty() {
                        msg
                    } else {
                        format!("{}\n\n{}", output_buf, msg)
                    };
                    update_message(&state, &activity, sid, &display).await;
                }
            }
            CoreEvent::Status(s) => {
                tracing::debug!(status = %s, "Teams core status");
            }
            CoreEvent::Error(e) => {
                tracing::error!(error = %e, "Teams core error");
                break;
            }
        }
    }

    // Send final output
    if !output_buf.is_empty() {
        for chunk in chunk_message(&output_buf, 28_000) {
            reply_text(&state, &activity, chunk).await;
        }
        // Delete status message if we have one and output was sent separately
        if let Some(ref sid) = status_id {
            if output_buf.len() > 500 {
                let bearer = get_bearer(&state).await.unwrap_or_default();
                let conv_id = activity.conversation_id();
                let url = format!("{}v3/conversations/{}/activities/{}", activity.service_url, conv_id, sid);
                let _ = state.http.delete(&url).bearer_auth(&bearer).send().await;
            }
        }
    } else if let Some(ref sid) = status_id {
        update_message(&state, &activity, sid, "_Done._").await;
    }

    if let Some(followup) = followup {
        reply_text(&state, &activity, &followup).await;
    }
}

// ── Commands ──────────────────────────────────────────────────────────

async fn command_help(state: &TeamsState, activity: &Activity) {
    let text = "**Sparks Commands**\n\n\
        Type a message or @mention me, or use `/sparks <command>`:\n\n\
        - `run <task>` - dispatch a task\n\
        - `plan` - start a planning interview\n\
        - `status` - show system status\n\
        - `memory <query>` - search memory\n\
        - `review [summary|standard|detailed] [hours]` - session review\n\
        - `explain [detail] [hours]` - AI explanation of recent activity\n\
        - `search <query>` - search session history\n\
        - `alerts [list|add|remove|toggle]` - manage alert rules\n\
        - `health` - run diagnostics\n\
        - `help` - show this message";
    reply_text(state, activity, text).await;
}

async fn command_status(state: &TeamsState, activity: &Activity) {
    let uptime = format_duration(state.system_info.started_at);
    let text = format!(
        "**Sparks Status**\n\n\
        - Uptime: {}\n\
        - Provider: {}\n\
        - Temperature: {:.2}\n\
        - Max tokens: {}\n\
        - Teams mode: active",
        uptime,
        state.system_info.provider,
        state.system_info.temperature,
        state.system_info.max_tokens,
    );
    reply_text(state, activity, &text).await;
}

async fn command_memory(state: &TeamsState, activity: &Activity, query: &str) {
    if query.is_empty() {
        reply_text(state, activity, "Usage: `memory <query>`").await;
        return;
    }
    let results = match state.handle.memory.search(query) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Memory search failed");
            reply_text(state, activity, "_Memory search failed._").await;
            return;
        }
    };
    if results.is_empty() {
        reply_text(state, activity, &format!("No memory entries found for \"{}\".", escape_teams(query))).await;
        return;
    }
    let mut text = format!("**Memory: \"{}\"**\n\n", escape_teams(query));
    for (i, r) in results.iter().enumerate() {
        let preview = &r.content[..r.content.len().min(200)];
        text.push_str(&format!("{}. {}\n", i + 1, escape_teams(preview)));
    }
    reply_text(state, activity, &text).await;
}

async fn command_review(
    state: &TeamsState,
    activity: &Activity,
    user_id: &str,
    arg: &str,
) {
    use crate::session_review::{render_review_mrkdwn, ReviewDetail};

    let args: Vec<&str> = arg.split_whitespace().collect();
    let detail = args.first().map(|a| ReviewDetail::from_str_loose(a)).unwrap_or(ReviewDetail::Standard);
    let hours: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(24);

    let conv_id = activity.conversation_id();
    let session_key = format!("teams:{}:{}", user_id, conv_id);
    let entries = state.handle.activity_log.recent(&session_key, 200).unwrap_or_default();
    let auto_entries = state.handle.activity_log.recent("autonomous", 100).unwrap_or_default();
    let mut all_entries = entries;
    all_entries.extend(auto_entries);
    all_entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
    let filtered: Vec<_> = all_entries.into_iter().filter(|e| e.created_at >= cutoff_str).collect();

    let review = render_review_mrkdwn(&filtered, detail);
    reply_text(state, activity, &review).await;
}

async fn command_search(
    state: &TeamsState,
    activity: &Activity,
    arg: &str,
) {
    use crate::session_review::render_search_results_mrkdwn;

    if arg.is_empty() {
        reply_text(state, activity, "Usage: `search <query>`").await;
        return;
    }
    let entries = state.handle.activity_log.search(arg, 30).unwrap_or_default();
    let text = render_search_results_mrkdwn(&entries, arg);
    reply_text(state, activity, &text).await;
}

async fn command_alerts(state: &TeamsState, activity: &Activity, arg: &str) {
    use crate::session_review::render_alert_rules_mrkdwn;

    let words: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = words.first().copied().unwrap_or("list");

    match subcmd {
        "list" | "" => {
            let rules = state.handle.activity_log.list_alert_rules().unwrap_or_default();
            reply_text(state, activity, &render_alert_rules_mrkdwn(&rules)).await;
        }
        "add" if words.len() >= 3 => {
            let name = words[1];
            let pattern = words[2];
            let target = words.get(3).copied().unwrap_or("any");
            let severity = words.get(4).copied().unwrap_or("info");
            match state.handle.activity_log.add_alert_rule(name, pattern, target, severity, None) {
                Ok(_) => reply_text(state, activity, &format!("Alert rule \"{}\" added.", name)).await,
                Err(e) => reply_text(state, activity, &format!("Failed to add alert: {}", e)).await,
            }
        }
        "remove" if words.len() >= 2 => {
            if let Ok(id) = words[1].parse::<i64>() {
                match state.handle.activity_log.remove_alert_rule(id) {
                    Ok(_) => reply_text(state, activity, &format!("Alert rule #{} removed.", id)).await,
                    Err(e) => reply_text(state, activity, &format!("Failed to remove alert: {}", e)).await,
                }
            } else {
                reply_text(state, activity, "Usage: `alerts remove <id>`").await;
            }
        }
        "toggle" if words.len() >= 2 => {
            if let Ok(id) = words[1].parse::<i64>() {
                // toggle: fetch current state and flip it
                let rules = state.handle.activity_log.list_alert_rules().unwrap_or_default();
                if let Some(rule) = rules.iter().find(|r| r.id == id) {
                    let new_state = !rule.enabled;
                    match state.handle.activity_log.toggle_alert_rule(id, new_state) {
                        Ok(_) => reply_text(state, activity, &format!("Alert rule #{} toggled.", id)).await,
                        Err(e) => reply_text(state, activity, &format!("Failed to toggle alert: {}", e)).await,
                    }
                } else {
                    reply_text(state, activity, &format!("Alert rule #{} not found.", id)).await;
                }
            } else {
                reply_text(state, activity, "Usage: `alerts toggle <id>`").await;
            }
        }
        _ => {
            reply_text(state, activity, "Usage: `alerts [list|add <name> <pattern> [target] [severity]|remove <id>|toggle <id>]`").await;
        }
    }
}

async fn command_health(state: &TeamsState, activity: &Activity) {
    reply_text(state, activity, "_Running health checks..._").await;

    let bearer_ok = get_bearer(state).await.is_ok();
    let memory_ok = state.handle.memory.search("health_probe_xyz").is_ok();

    let bearer_status = if bearer_ok { "**OK**" } else { "**Failed** (check app_id/app_password)" };
    let memory_status = if memory_ok { "**Available**" } else { "**Unavailable**" };
    let text = format!(
        "**Health Check**\n\n\
        - Teams connection: **Active** (you sent this message)\n\
        - Outbound auth (Bearer token): {}\n\
        - Core handle: **Active**\n\
        - Memory store: {}",
        bearer_status,
        memory_status,
    );
    reply_text(state, activity, &text).await;
}

async fn command_run(
    state: &TeamsState,
    activity: &Activity,
    user_id: &str,
    text: &str,
) {
    if text.is_empty() {
        reply_text(state, activity, "Usage: `run <task description>`").await;
        return;
    }
    let conv_id = activity.conversation_id();
    let session_ctx = SessionContext {
        platform: "teams".into(),
        user_id: user_id.to_string(),
        chat_id: conv_id.to_string(),
    };
    dispatch_to_core(state, activity.clone(), session_ctx, text.to_string(), "_Running..._", None).await;
}

// ── Planning ──────────────────────────────────────────────────────────

async fn handle_planning_start(state: &TeamsState, activity: &Activity) {
    let key = activity.conversation_id().to_string();
    let interview = PlanningInterview::new();
    {
        let mut planning = state.planning.lock().await;
        planning.insert(key, interview);
    }
    reply_card(state, activity, build_planning_start_card()).await;
}

async fn handle_planning_invoke(state: &TeamsState, activity: &Activity) {
    let value = match &activity.value {
        Some(v) => v.clone(),
        None => return,
    };

    let action = value["action"].as_str().unwrap_or("");
    if action != "planning" {
        return;
    }

    let step = value["step"].as_str().unwrap_or("");
    let val = value["value"].as_str().unwrap_or("");
    let key = activity.conversation_id().to_string();
    let user_id = activity.from_id().to_string();
    let conv_id = activity.conversation_id().to_string();

    let mut planning = state.planning.lock().await;
    let Some(interview) = planning.get_mut(&key) else {
        drop(planning);
        reply_text(state, activity, "_No active planning session. Type 'plan' to start._").await;
        return;
    };
    interview.last_updated = tokio::time::Instant::now();

    // Validate step ordering
    let step_str = step;
    let expected = match interview.step {
        PlanningStep::Goal => "goal",
        PlanningStep::Constraints => "constraints_submit",
        PlanningStep::Output => "output",
        PlanningStep::Summary | PlanningStep::Done => {
            drop(planning);
            reply_text(state, activity, "_Planning session is complete. Type 'plan' to start a new one._").await;
            return;
        }
    };
    // Allow selection steps (timeline, scope, depth) at any constraints/output phase
    let is_selection = matches!(step_str, "timeline" | "scope" | "depth" | "output");
    if !is_selection && step_str != expected {
        drop(planning);
        reply_text(state, activity, &format!("_Unexpected planning step. Expected '{}'._", expected)).await;
        return;
    }

    match step {
        "goal" => {
            interview.goal = value["goal"].as_str().map(str::to_string)
                .or_else(|| if !val.is_empty() { Some(val.to_string()) } else { None });
            interview.step = PlanningStep::Constraints;
            drop(planning);
            reply_card(state, activity, build_planning_constraints_card()).await;
        }
        "timeline" => {
            interview.timeline = Some(val.to_string());
            drop(planning);
        }
        "scope" => {
            interview.scope = Some(val.to_string());
            drop(planning);
        }
        "constraints_submit" => {
            let constraints = value["constraints"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            interview.constraints = constraints;
            if interview.timeline.is_none() {
                interview.timeline = Some("unspecified".to_string());
            }
            if interview.scope.is_none() {
                interview.scope = Some("unspecified".to_string());
            }
            interview.step = PlanningStep::Output;
            drop(planning);
            reply_card(state, activity, build_planning_output_card()).await;
        }
        "output" => {
            interview.output = Some(val.to_string());
            drop(planning);
        }
        "depth" => {
            interview.depth = Some(val.to_string());
            // Both output and depth set - show summary and generate
            let has_output = interview.output.is_some();
            if has_output {
                interview.step = PlanningStep::Summary;
                let summary = planning_summary(interview);
                let prompt = planning_build_prompt(interview);
                planning.remove(&key);
                drop(planning);

                reply_text(state, activity, &summary).await;
                let session_ctx = SessionContext {
                    platform: "teams".into(),
                    user_id: user_id.clone(),
                    chat_id: conv_id.clone(),
                };
                dispatch_to_core(state, activity.clone(), session_ctx, prompt, "_Generating plan..._", None).await;
            } else {
                drop(planning);
            }
        }
        _ => {
            drop(planning);
            reply_text(state, activity, "_Unknown planning step._").await;
        }
    }
}

// ── Invoke handler (Adaptive Card actions) ────────────────────────────

async fn handle_invoke(state: TeamsState, activity: Activity) {
    let tenant_id = activity.tenant_id();
    if !is_authorized(tenant_id.as_deref(), &state.config) {
        tracing::debug!(tenant_id = ?tenant_id, "Ignoring invoke from unauthorized tenant");
        return;
    }

    let value = match &activity.value {
        Some(v) => v.clone(),
        None => return,
    };

    let action = value["action"].as_str().unwrap_or("");

    match action {
        "confirm" => {
            let confirm_id = value["confirm_id"].as_str().unwrap_or("").to_string();
            let approved = value["value"].as_bool().unwrap_or(false);
            let mut pending = state.pending.lock().await;
            if let Some(tx) = pending.remove(&confirm_id) {
                let _ = tx.send(approved);
            }
        }
        "planning" => {
            handle_planning_invoke(&state, &activity).await;
        }
        _ => {
            tracing::debug!(action, "Unknown Teams invoke action");
        }
    }
}

// ── Message handler ───────────────────────────────────────────────────

async fn handle_message(state: TeamsState, activity: Activity) {
    let user_id = activity.from_id().to_string();
    let conv_id = activity.conversation_id().to_string();
    let tenant_id = activity.tenant_id();

    // Authorization
    if !is_authorized(tenant_id.as_deref(), &state.config) {
        tracing::debug!(tenant_id = ?tenant_id, "Ignoring message from unauthorized tenant");
        return;
    }

    // Rate limiting per user+conversation
    let rate_key = format!("{}:{}", user_id, conv_id);
    {
        let now = tokio::time::Instant::now();
        let mut map = state.last_request.lock().await;
        if should_rate_limit(
            map.get(&rate_key).copied(),
            now,
            tokio::time::Duration::from_secs(state.config.rate_limit_secs),
        ) {
            tracing::debug!("Teams rate limit hit for {}", rate_key);
            return;
        }
        map.insert(rate_key.clone(), now);
    }

    let raw_text = activity.text.clone().unwrap_or_default();
    // Strip <at>...</at> @mention tags that Teams injects into the message text
    let text = {
        let stripped = raw_text.trim();
        RE_AT_MENTION.replace_all(stripped, "").trim().to_string()
    };

    if text.is_empty() {
        return;
    }

    // Parse /sparks command prefix or bare commands
    let (cmd, arg): (&str, &str) = if let Some(rest) = text.strip_prefix("/sparks ").or_else(|| if text == "/sparks" { Some("") } else { None }) {
        let rest = rest.trim();
        match rest.split_once(' ') {
            Some((c, a)) => (c.trim(), a.trim()),
            None => (rest, ""),
        }
    } else {
        // Treat whole message as a run/chat unless it's a known bare command
        match text.split_once(' ') {
            Some((c, a)) if matches!(c, "help" | "status" | "run" | "plan" | "memory" | "review" | "explain" | "search" | "alerts" | "health") => (c, a.trim()),
            None if matches!(text.as_str(), "help" | "status" | "plan" | "health") => (text.as_str(), ""),
            _ => ("chat", text.as_str()),
        }
    };

    match cmd {
        "help" => command_help(&state, &activity).await,
        "status" => command_status(&state, &activity).await,
        "health" => command_health(&state, &activity).await,
        "memory" => command_memory(&state, &activity, arg).await,
        "review" => command_review(&state, &activity, &user_id, arg).await,
        "search" => command_search(&state, &activity, arg).await,
        "alerts" => command_alerts(&state, &activity, arg).await,
        "run" => command_run(&state, &activity, &user_id, arg).await,
        "plan" => handle_planning_start(&state, &activity).await,
        "explain" => {
            use crate::session_review::{generate_explanation, ReviewDetail};
            let args: Vec<&str> = arg.split_whitespace().collect();
            let detail = args.first().map(|a| ReviewDetail::from_str_loose(a)).unwrap_or(ReviewDetail::Standard);
            let hours: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(24);
            let session_key = format!("teams:{}:{}", user_id, conv_id);
            let entries = state.handle.activity_log.recent(&session_key, 200).unwrap_or_default();
            let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
            let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
            let filtered: Vec<_> = entries.into_iter().filter(|e| e.created_at >= cutoff_str).collect();
            match generate_explanation(&filtered, state.handle.llm.as_ref(), detail).await {
                Ok(exp) => reply_text(&state, &activity, &format!("**Session Explanation**\n\n{}", exp)).await,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to generate explanation");
                    reply_text(&state, &activity, "_Failed to generate explanation._").await;
                }
            }
        }
        "chat" | _ => {
            // Check if planning-like and auto-start planning
            if state.config.planning_enabled && state.config.planning_auto && is_planning_like(&text) {
                let already_planning = state.planning.lock().await.contains_key(&conv_id);
                if !already_planning {
                    handle_planning_start(&state, &activity).await;
                    return;
                }
            }
            let session_ctx = SessionContext {
                platform: "teams".into(),
                user_id: user_id.clone(),
                chat_id: conv_id.clone(),
            };
            dispatch_to_core(&state, activity.clone(), session_ctx, text, "_Thinking..._", None).await;
        }
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────

async fn handle_messages(
    State(state): State<TeamsState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Validate token
    if let Err(e) = validate_token(&headers, &state.app_id, state.config.skip_auth, &state.http).await {
        tracing::warn!(error = %e, "Teams auth failed");
        return StatusCode::UNAUTHORIZED;
    }

    let activity: Activity = match serde_json::from_slice(&body) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse Teams activity");
            return StatusCode::BAD_REQUEST;
        }
    };

    tracing::debug!(activity_type = %activity.activity_type, "Teams activity received");

    match activity.activity_type.as_str() {
        "message" => {
            tokio::spawn(handle_message(state, activity));
        }
        "invoke" => {
            tokio::spawn(handle_invoke(state, activity));
        }
        "conversationUpdate" => {
            tracing::info!("Teams conversationUpdate received");
        }
        other => {
            tracing::debug!(activity_type = other, "Ignoring Teams activity type");
        }
    }

    StatusCode::OK
}

async fn handle_health() -> impl IntoResponse {
    "ok"
}

// ── Entry point ───────────────────────────────────────────────────────

/// Start the Teams bot. Called from main.rs.
pub async fn run_teams(
    handle: CoreHandle,
    config: TeamsConfig,
    system_info: SystemInfo,
) -> anyhow::Result<()> {
    let app_id = config
        .app_id
        .clone()
        .or_else(|| std::env::var("SPARKS_TEAMS_APP_ID").ok())
        .ok_or_else(|| anyhow::anyhow!(
            "Teams requires app_id. Set [teams].app_id or SPARKS_TEAMS_APP_ID env var"
        ))?;

    let app_password = config
        .app_password
        .clone()
        .or_else(|| std::env::var("SPARKS_TEAMS_APP_PASSWORD").ok())
        .ok_or_else(|| anyhow::anyhow!(
            "Teams requires app_password. Set [teams].app_password or SPARKS_TEAMS_APP_PASSWORD env var"
        ))?;

    // Safety guard: must explicitly allow tenants or set allow_all_tenants
    if config.allowed_tenants.is_empty() && !config.allow_all_tenants {
        return Err(anyhow::anyhow!(
            "Teams bot will not start: no allowed_tenants configured and allow_all_tenants is false. \
             Set [teams].allowed_tenants or [teams].allow_all_tenants = true"
        ));
    }

    if config.allow_all_tenants && config.allowed_tenants.is_empty() {
        tracing::warn!("Teams bot: allow_all_tenants = true — any tenant can interact with this bot");
    }

    if config.skip_auth {
        tracing::warn!("Teams bot: skip_auth = true — JWT validation is DISABLED, not safe for production");
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let state = TeamsState {
        handle,
        http,
        app_id: app_id.clone(),
        app_password,
        pending: Arc::new(Mutex::new(HashMap::new())),
        last_request: Arc::new(Mutex::new(HashMap::new())),
        planning: Arc::new(Mutex::new(HashMap::new())),
        config: config.clone(),
        system_info,
        bearer: Arc::new(Mutex::new(None)),
        jwks_cache: Arc::new(Mutex::new(None)),
    };

    // Cleanup task: remove stale pending confirmations and planning sessions
    let cleanup_pending = state.pending.clone();
    let cleanup_planning = state.planning.clone();
    let planning_timeout = tokio::time::Duration::from_secs(config.planning_timeout_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = tokio::time::Instant::now();
            cleanup_pending.lock().await.retain(|_, tx| !tx.is_closed());
            cleanup_planning.lock().await.retain(|_, iv| {
                now.duration_since(iv.last_updated) < planning_timeout
            });
        }
    });

    let bind_addr: std::net::SocketAddr = config
        .bind_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid teams.bind_addr: {}", e))?;

    let app = Router::new()
        .route("/api/messages", post(handle_messages))
        .route("/api/health", axum::routing::get(handle_health))
        .with_state(state);

    tracing::info!("Teams bot starting on {}", bind_addr);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_message_basic() {
        let text = "hello world";
        let chunks = chunk_message(text, 5);
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn chunk_message_exact() {
        let text = "abcde";
        let chunks = chunk_message(text, 5);
        assert_eq!(chunks, vec!["abcde"]);
    }

    #[test]
    fn chunk_message_unicode() {
        let text = "héllo";
        let chunks = chunk_message(text, 3);
        for c in &chunks {
            assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        }
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn escape_teams_html() {
        assert_eq!(escape_teams("a & b <c> d"), "a &amp; b &lt;c&gt; d");
    }

    #[test]
    fn is_authorized_allowlist_wins() {
        let config = TeamsConfig {
            allowed_tenants: vec!["t1".into()],
            allow_all_tenants: true,
            ..Default::default()
        };
        assert!(is_authorized(Some("t1"), &config));
        assert!(!is_authorized(Some("t2"), &config));
        assert!(!is_authorized(None, &config));
    }

    #[test]
    fn is_authorized_allow_all() {
        let config = TeamsConfig {
            allowed_tenants: vec![],
            allow_all_tenants: true,
            ..Default::default()
        };
        assert!(is_authorized(Some("any-tenant"), &config));
    }

    #[test]
    fn is_authorized_deny_all() {
        let config = TeamsConfig::default();
        assert!(!is_authorized(Some("any-tenant"), &config));
    }

    #[test]
    fn should_rate_limit_within_window() {
        let now = tokio::time::Instant::now();
        assert!(should_rate_limit(Some(now), now, tokio::time::Duration::from_secs(5)));
    }

    #[test]
    fn is_planning_like_detects_keywords() {
        assert!(is_planning_like("I need to plan a new feature"));
        assert!(is_planning_like("Create a roadmap for Q2"));
        assert!(!is_planning_like("What time is it?"));
    }

    #[test]
    fn planning_summary_formats_fields() {
        let interview = PlanningInterview {
            goal: Some("Build X".into()),
            timeline: Some("this week".into()),
            scope: Some("implementation".into()),
            constraints: Some("no budget".into()),
            output: Some("checklist".into()),
            depth: Some("standard".into()),
            step: PlanningStep::Done,
            last_updated: tokio::time::Instant::now(),
        };
        let s = planning_summary(&interview);
        assert!(s.contains("Build X"));
        assert!(s.contains("this week"));
        assert!(s.contains("checklist"));
    }

    #[test]
    fn format_duration_seconds() {
        let inst = tokio::time::Instant::now();
        let _ = format_duration(inst);
    }

    #[test]
    fn activity_tenant_id_from_channel_data() {
        let activity = Activity {
            activity_type: "message".into(),
            id: String::new(),
            timestamp: String::new(),
            from: None,
            recipient: None,
            conversation: None,
            service_url: String::new(),
            channel_id: "msteams".into(),
            text: Some("hello".into()),
            reply_to_id: String::new(),
            channel_data: Some(json!({ "tenant": { "id": "tenant-abc" } })),
            value: None,
            attachments: None,
        };
        assert_eq!(activity.tenant_id(), Some("tenant-abc".to_string()));
    }
}
