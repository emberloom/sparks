use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use slack_morphism::prelude::*;

use crate::config::SlackConfig;
use crate::confirm::Confirmer;
use crate::core::{CoreEvent, CoreHandle, SessionContext};
use crate::error::{SparksError, Result};

use crate::session_review::{ActivityEntry, ActivityLogStore};

/// System info passed from main to the Slack bot.
#[derive(Clone)]
pub struct SystemInfo {
    pub provider: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub started_at: tokio::time::Instant,
}

// ── Planning types (mirrored from telegram.rs) ──────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlanningStep {
    Goal,
    Constraints,
    Output,
    Summary,
    Editing,
    Refining,
}

#[derive(Clone, Debug)]
struct PlanningInterview {
    step: PlanningStep,
    goal: Option<String>,
    constraints: Option<String>,
    output: Option<String>,
    timeline: Option<String>,
    depth: Option<String>,
    scope: Option<String>,
    last_updated: tokio::time::Instant,
}

impl PlanningInterview {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            step: PlanningStep::Goal,
            goal: None,
            constraints: None,
            output: None,
            timeline: None,
            depth: None,
            scope: None,
            last_updated: now,
        }
    }
}

// ── mrkdwn formatting helpers ───────────────────────────────────────

/// Escape text for safe inclusion in Slack mrkdwn.
fn escape_mrkdwn(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('*', "\u{2217}")  // asterisk operator (prevents bold)
        .replace('_', "\u{FF3F}")  // fullwidth low line (prevents italic)
        .replace('~', "\u{223C}")  // tilde operator (prevents strikethrough)
        .replace('`', "\u{2018}")  // left single quotation (prevents code)
}

/// Format a duration in seconds as human-readable (e.g., "2h 15m").
fn format_duration(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

/// Format a token count as human-readable (e.g., "128k", "1.2M").
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let m = tokens as f64 / 1_000_000.0;
        if m == m.trunc() {
            format!("{}M", m as u64)
        } else {
            format!("{:.1}M", m)
        }
    } else if tokens >= 1_000 {
        let k = tokens as f64 / 1_000.0;
        if k == k.trunc() {
            format!("{}k", k as u64)
        } else {
            format!("{:.1}k", k)
        }
    } else {
        tokens.to_string()
    }
}

/// Build a visual energy bar like [████████░░].
fn energy_bar(energy: f32) -> String {
    let filled = (energy * 10.0).round() as usize;
    let empty = 10usize.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

/// Split a message into chunks at `max` bytes, respecting UTF-8 char boundaries.
fn chunk_message(text: &str, max: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + max).min(text.len());
        let mut actual_end = end;
        while actual_end > start && !text.is_char_boundary(actual_end) {
            actual_end -= 1;
        }
        if actual_end < text.len() {
            if let Some(pos) = text[start..actual_end].rfind('\n') {
                actual_end = start + pos + 1;
            }
        }
        chunks.push(&text[start..actual_end]);
        start = actual_end;
    }
    chunks
}

/// Truncate streaming buffer for preview display.
fn stream_preview(buffer: &str) -> String {
    let preview = escape_mrkdwn(buffer);
    if preview.len() > 2800 {
        format!("{}...", &preview[..preview.floor_char_boundary(2800)])
    } else {
        preview
    }
}

/// Format a tool result as mrkdwn text.
fn tool_result_mrkdwn(tool: &str, result: &str, success: bool) -> String {
    let icon = if success { ":white_check_mark:" } else { ":x:" };
    let body = result
        .strip_prefix("[tool result]\n")
        .or_else(|| result.strip_prefix("[tool error]\n"))
        .unwrap_or(result);
    let display = if body.len() > 1500 {
        format!(
            "{}...\n_[{} chars total]_",
            escape_mrkdwn(&body[..body.floor_char_boundary(1500)]),
            body.len()
        )
    } else {
        escape_mrkdwn(body)
    };
    format!(
        "{} *Behind the scenes — {}*\n```{}```",
        icon,
        escape_mrkdwn(tool),
        display,
    )
}

/// Send a mrkdwn-formatted message, chunking if needed.
async fn send_mrkdwn(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    text: &str,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for chunk in chunk_message(text, 3000) {
        let content = SlackMessageContent::new().with_text(chunk.to_string());
        let mut req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts.clone());
        }
        session.chat_post_message(&req).await?;
    }
    Ok(())
}

/// Send response text in chunks.
async fn send_response_chunks(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    response_text: &str,
) {
    let escaped = escape_mrkdwn(response_text);
    for chunk in chunk_message(&escaped, 3000) {
        let content = SlackMessageContent::new().with_text(chunk.to_string());
        let mut req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts.clone());
        }
        let _ = session.chat_post_message(&req).await;
    }
}

// ── Confirmer ────────────────────────────────────────────────────────

/// Slack confirmer: sends Block Kit buttons, waits on oneshot with timeout.
pub struct SlackConfirmer {
    client: Arc<SlackHyperClient>,
    bot_token: SlackApiToken,
    channel: SlackChannelId,
    thread_ts: Option<SlackTs>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout_secs: u64,
}

#[async_trait::async_trait]
impl Confirmer for SlackConfirmer {
    async fn confirm(&self, action: &str) -> Result<bool> {
        let confirm_id = uuid::Uuid::new_v4().to_string();

        let blocks = vec![
            SlackBlock::Section(
                SlackSectionBlock::new().with_text(SlackBlockText::MarkDown(
                    SlackBlockMarkDownText::new(format!(
                        "*Action requires approval:*\n```{}```",
                        escape_mrkdwn(action)
                    )),
                )),
            ),
            SlackBlock::Actions(SlackActionsBlock::new(vec![
                SlackActionBlockElement::Button(
                    SlackBlockButtonElement::new(
                        SlackActionId::from(format!("confirm:{}:yes", confirm_id)),
                        SlackBlockPlainTextOnly::from("Approve"),
                    )
                    .with_style("primary".to_string()),
                ),
                SlackActionBlockElement::Button(
                    SlackBlockButtonElement::new(
                        SlackActionId::from(format!("confirm:{}:no", confirm_id)),
                        SlackBlockPlainTextOnly::from("Deny"),
                    )
                    .with_style("danger".to_string()),
                ),
            ])),
        ];

        let session = self.client.open_session(&self.bot_token);
        let content = SlackMessageContent::new().with_blocks(blocks);
        let mut req = SlackApiChatPostMessageRequest::new(self.channel.clone(), content);
        if let Some(ts) = &self.thread_ts {
            req = req.with_thread_ts(ts.clone());
        }
        session
            .chat_post_message(&req)
            .await
            .map_err(|e| SparksError::Tool(format!("Failed to send confirmation: {}", e)))?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(confirm_id.clone(), tx);
        }

        let timeout = tokio::time::Duration::from_secs(self.timeout_secs);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(true)) => Ok(true),
            Ok(Ok(false)) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&confirm_id);
                Err(SparksError::Denied)
            }
            Err(_) | Ok(Err(_)) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&confirm_id);
                Err(SparksError::Cancelled)
            }
        }
    }
}

// ── State ────────────────────────────────────────────────────────────

/// Shared state for the Slack bot.
#[derive(Clone)]
struct SlackState {
    handle: CoreHandle,
    client: Arc<SlackHyperClient>,
    bot_token: SlackApiToken,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    config: SlackConfig,
    last_request: Arc<Mutex<HashMap<String, tokio::time::Instant>>>,
    planning: Arc<Mutex<HashMap<String, PlanningInterview>>>,
    system_info: SystemInfo,
}

/// Check if a channel is authorized.
fn is_authorized(channel_id: &str, config: &SlackConfig) -> bool {
    // allowed_channels takes precedence over allow_all for safety.
    // If both are set, only channels in allowed_channels are accepted.
    if !config.allowed_channels.is_empty() {
        return config.allowed_channels.contains(&channel_id.to_string());
    }
    config.allow_all
}

fn should_rate_limit(
    last_request: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    cooldown: tokio::time::Duration,
) -> bool {
    match last_request {
        Some(last) => now.duration_since(last) < cooldown,
        None => false,
    }
}

fn slack_confirmer(state: &SlackState, channel: &SlackChannelId, thread_ts: Option<&SlackTs>) -> Arc<dyn Confirmer> {
    Arc::new(SlackConfirmer {
        client: state.client.clone(),
        bot_token: state.bot_token.clone(),
        channel: channel.clone(),
        thread_ts: thread_ts.cloned(),
        pending: state.pending.clone(),
        timeout_secs: state.config.confirm_timeout_secs,
    })
}

fn slack_session(user_id: &str, channel_id: &str) -> SessionContext {
    SessionContext {
        platform: "slack".into(),
        user_id: user_id.to_string(),
        chat_id: channel_id.to_string(),
    }
}

/// Composite key for thread-scoped state maps.
fn thread_key(channel: &str, thread_ts: Option<&SlackTs>) -> String {
    match thread_ts {
        Some(ts) => format!("{}:{}", channel, ts),
        None => channel.to_string(),
    }
}

// ── Planning interview helpers ──────────────────────────────────────

const PLAN_KEYWORDS: &[&str] = &[
    "plan", "planning", "roadmap", "strategy", "launch", "rollout", "gtm",
];

fn should_start_planning_interview(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.starts_with('/') {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return false;
    }
    if words.iter().any(|w| PLAN_KEYWORDS.contains(w)) {
        return true;
    }
    lowered.contains("help me plan") || lowered.contains("need a plan")
}

fn is_bare_plan_request(text: &str) -> bool {
    let lowered = text.trim().to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return true;
    }
    if words.len() <= 2 && words.iter().any(|w| *w == "plan" || *w == "planning") {
        return true;
    }
    matches!(
        lowered.as_str(),
        "plan" | "planning" | "make a plan" | "need a plan"
    )
}

fn is_cancel_text(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "cancel" | "stop" | "never mind" | "nevermind" | "abort"
    )
}

fn is_confirm_text(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "yes" | "y" | "ok" | "okay" | "confirm" | "looks good" | "go ahead" | "proceed"
    )
}

fn is_edit_text(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "edit" | "change" | "update" | "revise"
    )
}

fn is_skip_text(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "skip" | "pass")
}

fn is_done_text(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "done" | "finished" | "looks good" | "that's it" | "thats it" | "perfect"
    )
}

fn apply_planning_edits(interview: &mut PlanningInterview, text: &str) -> bool {
    let mut changed = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_lowercase().as_str() {
            "goal" => {
                interview.goal = Some(value.to_string());
                changed = true;
            }
            "constraints" => {
                interview.constraints = Some(value.to_string());
                changed = true;
            }
            "timeline" => {
                interview.timeline = Some(value.to_string());
                changed = true;
            }
            "depth" => {
                interview.depth = Some(value.to_string());
                changed = true;
            }
            "scope" => {
                interview.scope = Some(value.to_string());
                changed = true;
            }
            "output" | "format" => {
                interview.output = Some(value.to_string());
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

fn planning_value_label(kind: &str, value: &str) -> Option<&'static str> {
    match (kind, value) {
        ("depth", "quick") => Some("quick"),
        ("depth", "standard") => Some("standard"),
        ("depth", "deep") => Some("deep"),
        ("timeline", "today") => Some("today"),
        ("timeline", "week") => Some("this week"),
        ("timeline", "none") => Some("no deadline"),
        ("scope", "idea") => Some("idea only"),
        ("scope", "impl") => Some("implementation"),
        ("scope", "full") => Some("full plan"),
        ("output", "checklist") => Some("checklist"),
        ("output", "spec") => Some("spec"),
        ("output", "draft") => Some("draft"),
        ("constraints", "none") => Some("none"),
        _ => None,
    }
}

fn planning_build_prompt(interview: &PlanningInterview) -> String {
    let mut out = String::from("Create a plan with the following inputs:\n\n");
    if let Some(goal) = &interview.goal {
        out.push_str(&format!("Goal: {}\n", goal));
    } else {
        out.push_str("Goal: unspecified\n");
    }
    if let Some(constraints) = &interview.constraints {
        out.push_str(&format!("Constraints: {}\n", constraints));
    } else {
        out.push_str("Constraints: none\n");
    }
    if let Some(timeline) = &interview.timeline {
        out.push_str(&format!("Timeline: {}\n", timeline));
    } else {
        out.push_str("Timeline: unspecified\n");
    }
    if let Some(scope) = &interview.scope {
        out.push_str(&format!("Scope: {}\n", scope));
    } else {
        out.push_str("Scope: unspecified\n");
    }
    let depth = interview.depth.as_deref().unwrap_or("standard");
    out.push_str(&format!("Depth: {}\n", depth));
    let output = interview.output.as_deref().unwrap_or("checklist");
    out.push_str(&format!("Output format: {}\n", output));
    out.push_str("\nReturn a clear, step-by-step plan.");
    out.push_str("\nAt the very end, add a short '## Summary' section (3-4 bullet points) recapping the key deliverables.");
    out.push_str(
        "\nFinish with: 'Ready to implement this plan, or would you like to refine anything?'",
    );
    out
}

fn planning_summary_mrkdwn(interview: &PlanningInterview) -> String {
    let goal = interview.goal.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("unspecified");
    let constraints = interview.constraints.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("none");
    let output = interview.output.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("checklist");
    let timeline = interview.timeline.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("unspecified");
    let depth = interview.depth.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("standard");
    let scope = interview.scope.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or("unspecified");

    format!(
        "*Here's what I have so far:*\n\n• *Goal:* {}\n• *Constraints:* {}\n• *Timeline:* {}\n• *Scope:* {}\n• *Depth:* {}\n• *Output:* {}\n\nConfirm or edit anything above.",
        escape_mrkdwn(goal),
        escape_mrkdwn(constraints),
        escape_mrkdwn(timeline),
        escape_mrkdwn(scope),
        escape_mrkdwn(depth),
        escape_mrkdwn(output),
    )
}

fn build_implement_prompt(goal: &str, interview: Option<&PlanningInterview>) -> String {
    let mut out =
        String::from("Implement the following step by step using the CLI coding tool.\n\n");
    if let Some(iv) = interview {
        if let Some(g) = &iv.goal {
            out.push_str(&format!("Goal: {}\n", g));
        } else {
            out.push_str(&format!("Goal: {}\n", goal));
        }
        if let Some(c) = &iv.constraints {
            out.push_str(&format!("Constraints: {}\n", c));
        }
        if let Some(t) = &iv.timeline {
            out.push_str(&format!("Timeline: {}\n", t));
        }
        if let Some(s) = &iv.scope {
            out.push_str(&format!("Scope: {}\n", s));
        }
    } else {
        out.push_str(&format!("Goal: {}\n", goal));
    }
    out.push_str("\nInstructions:\n");
    out.push_str("- Start with the first actionable item.\n");
    out.push_str("- After completing each step, briefly report what was done.\n");
    out.push_str("- Continue until the goal is fully implemented.\n");
    out.push_str("- End with a summary of all changes made.\n");
    out
}

// ── CLI tool picker helpers ─────────────────────────────────────────

const CLI_TOOLS: &[(&str, &str)] = &[
    ("claude_code", "Claude Code"),
    ("codex", "Codex"),
    ("opencode", "OpenCode"),
];

fn cli_display_name(tool_id: &str) -> String {
    CLI_TOOLS
        .iter()
        .find(|(id, _)| *id == tool_id)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| tool_id.to_string())
}

// ── Planning action enum ────────────────────────────────────────────

/// Build Block Kit actions for planning constraints step.
fn planning_constraints_blocks() -> Vec<SlackBlock> {
    vec![
        SlackBlock::Section(
            SlackSectionBlock::new()
                .with_text(SlackBlockText::MarkDown(SlackBlockMarkDownText::new(
                    "Any constraints? Pick quick options or type your own:".into(),
                ))),
        ),
        SlackBlock::Actions(SlackActionsBlock::new(vec![
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:timeline:today".into(), pt("Today".into()))
                    .with_value("today".into()),
            ),
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:timeline:week".into(), pt("This week".into()))
                    .with_value("week".into()),
            ),
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:timeline:none".into(), pt("No deadline".into()))
                    .with_value("none".into()),
            ),
        ])),
        SlackBlock::Actions(SlackActionsBlock::new(vec![
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:scope:idea".into(), pt("Idea only".into()))
                    .with_value("idea".into()),
            ),
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:scope:impl".into(), pt("Implementation".into()))
                    .with_value("impl".into()),
            ),
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:scope:full".into(), pt("Full plan".into()))
                    .with_value("full".into()),
            ),
        ])),
        SlackBlock::Actions(SlackActionsBlock::new(vec![
            SlackActionBlockElement::Button(
                SlackBlockButtonElement::new("plan:skip:constraints".into(), pt("Skip".into()))
                    .with_value("skip".into()),
            ),
        ])),
    ]
}

/// Build Block Kit actions for planning output format step.
fn planning_output_blocks() -> Vec<SlackBlock> {
    vec![SlackBlock::Actions(SlackActionsBlock::new(vec![
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:output:checklist".into(), pt("Checklist".into()))
                .with_value("checklist".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:output:spec".into(), pt("Spec".into()))
                .with_value("spec".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:output:draft".into(), pt("Draft".into()))
                .with_value("draft".into()),
        ),
    ]))]
}

/// Build Block Kit actions for planning confirm step.
fn planning_confirm_blocks() -> Vec<SlackBlock> {
    vec![SlackBlock::Actions(SlackActionsBlock::new(vec![
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:confirm:yes".into(), pt("Confirm".into()))
                .with_value("yes".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:confirm:edit".into(), pt("Edit".into()))
                .with_value("edit".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:confirm:cancel".into(), pt("Cancel".into()))
                .with_value("cancel".into()),
        ),
    ]))]
}

/// Build Block Kit actions for post-plan-generation step.
fn planning_post_generate_blocks() -> Vec<SlackBlock> {
    vec![SlackBlock::Actions(SlackActionsBlock::new(vec![
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:post:implement".into(), pt("Implement".into()))
                .with_value("implement".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:post:refine".into(), pt("Refine".into()))
                .with_value("refine".into()),
        ),
        SlackActionBlockElement::Button(
            SlackBlockButtonElement::new("plan:post:done".into(), pt("Done".into()))
                .with_value("done".into()),
        ),
    ]))]
}

/// Helper to create a plain_text object for button labels.
fn pt(text: String) -> SlackBlockPlainTextOnly {
    SlackBlockPlainTextOnly::from(text)
}

/// Send a mrkdwn message with Block Kit blocks appended.
async fn send_mrkdwn_with_blocks(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    text: &str,
    blocks: Vec<SlackBlock>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let content = SlackMessageContent::new()
        .with_text(text.to_string())
        .with_blocks(blocks);
    let mut req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
    if let Some(ts) = thread_ts {
        req = req.with_thread_ts(ts.clone());
    }
    session.chat_post_message(&req).await?;
    Ok(())
}

enum PlanningAction {
    None,
    Prompt(String),
    PromptWithBlocks(String, Vec<SlackBlock>),
    Dispatch(String),
    Cancelled,
    Done,
}

// ── Event forwarding ────────────────────────────────────────────────

/// Forward CoreEvent stream to Slack, streaming via chat.update.
async fn forward_slack_events(
    client: Arc<SlackHyperClient>,
    bot_token: SlackApiToken,
    channel: SlackChannelId,
    thread_ts: Option<SlackTs>,
    status_ts: SlackTs,
    mut events: mpsc::Receiver<CoreEvent>,
) {
    let session = client.open_session(&bot_token);
    let mut stream_buffer = String::new();
    let mut last_edit = tokio::time::Instant::now();
    let edit_interval = tokio::time::Duration::from_millis(800);

    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Status(s) => {
                let content = SlackMessageContent::new()
                    .with_text(format!("_Status: {}_", escape_mrkdwn(&s)));
                let _ = session
                    .chat_update(&SlackApiChatUpdateRequest::new(
                        channel.clone(),
                        content,
                        status_ts.clone(),
                    ))
                    .await;
            }
            CoreEvent::StreamChunk(chunk) => {
                // Cap buffer at 100KB to prevent OOM from unbounded responses
                if stream_buffer.len() < 100_000 {
                    stream_buffer.push_str(&chunk);
                }
                let now = tokio::time::Instant::now();
                if now.duration_since(last_edit) >= edit_interval {
                    let display = stream_preview(&stream_buffer);
                    let content = SlackMessageContent::new()
                        .with_text(format!("```{}```", display));
                    let _ = session
                        .chat_update(&SlackApiChatUpdateRequest::new(
                            channel.clone(),
                            content,
                            status_ts.clone(),
                        ))
                        .await;
                    last_edit = now;
                }
            }
            CoreEvent::ToolRun {
                tool,
                result,
                success,
            } => {
                let text = tool_result_mrkdwn(&tool, &result, success);
                let _ = send_mrkdwn(&session, &channel, thread_ts.as_ref(), &text).await;
            }
            CoreEvent::Response(r) => {
                // Delete status message, then post final response
                let _ = session
                    .chat_delete(&SlackApiChatDeleteRequest::new(
                        channel.clone(),
                        status_ts.clone(),
                    ))
                    .await;
                let response_text = if !stream_buffer.is_empty() {
                    stream_buffer.clone()
                } else {
                    r
                };
                if response_text.trim().is_empty() {
                    break;
                }
                send_response_chunks(&session, &channel, thread_ts.as_ref(), &response_text).await;
            }
            CoreEvent::Error(e) => {
                tracing::error!(error = %e, channel = %channel, "Task error");
                let content = SlackMessageContent::new()
                    .with_text("_An error occurred while processing your request._".to_string());
                let _ = session
                    .chat_update(&SlackApiChatUpdateRequest::new(
                        channel.clone(),
                        content,
                        status_ts.clone(),
                    ))
                    .await;
            }
        }
    }
}

/// Dispatch a message to CoreHandle and forward events to Slack.
async fn dispatch_to_core(
    state: &SlackState,
    channel: SlackChannelId,
    thread_ts: Option<SlackTs>,
    session_ctx: SessionContext,
    text: String,
    initial_status: &str,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dispatch_to_core_with_followup(state, channel, thread_ts, session_ctx, text, initial_status, None).await
}

/// Dispatch to core with a followup message after completion.
async fn dispatch_to_core_with_followup(
    state: &SlackState,
    channel: SlackChannelId,
    thread_ts: Option<SlackTs>,
    session_ctx: SessionContext,
    text: String,
    initial_status: &str,
    followup: Option<String>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let confirmer = slack_confirmer(state, &channel, thread_ts.as_ref());
    let api_session = state.client.open_session(&state.bot_token);

    let content = SlackMessageContent::new().with_text(initial_status.to_string());
    let mut req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
    if let Some(ts) = &thread_ts {
        req = req.with_thread_ts(ts.clone());
    }
    let status_resp = api_session
        .chat_post_message(&req)
        .await
        .map_err(|e| format!("Failed to send status: {}", e))?;
    let status_ts = status_resp.ts;

    let events = match state.handle.chat(session_ctx, &text, confirmer).await {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!(error = %e, "Core dispatch failed");
            let content = SlackMessageContent::new()
                .with_text("_An internal error occurred._".to_string());
            let _ = api_session
                .chat_update(&SlackApiChatUpdateRequest::new(
                    channel.clone(),
                    content,
                    status_ts,
                ))
                .await;
            return Ok(());
        }
    };

    let client = state.client.clone();
    let bot_token = state.bot_token.clone();
    tokio::spawn(async move {
        forward_slack_events(
            client.clone(),
            bot_token.clone(),
            channel.clone(),
            thread_ts.clone(),
            status_ts,
            events,
        )
        .await;
        if let Some(msg) = followup {
            let session = client.open_session(&bot_token);
            let content = SlackMessageContent::new()
                .with_text(msg)
                .with_blocks(planning_post_generate_blocks());
            let mut req = SlackApiChatPostMessageRequest::new(channel, content);
            if let Some(ts) = &thread_ts {
                req = req.with_thread_ts(ts.clone());
            }
            let _ = session.chat_post_message(&req).await;
        }
    });

    Ok(())
}

// ── Planning state machine ──────────────────────────────────────────

fn planning_advance_step(
    interview: &mut PlanningInterview,
    text: &str,
    remove_chat: &mut bool,
) -> PlanningAction {
    match interview.step {
        PlanningStep::Goal => {
            if is_skip_text(text) {
                PlanningAction::Prompt(
                    "I need a goal to plan against. What does success look like?".to_string(),
                )
            } else {
                interview.goal = Some(text.to_string());
                interview.step = PlanningStep::Constraints;
                PlanningAction::PromptWithBlocks(
                    "Any constraints I should respect? Pick quick options or type your own:".to_string(),
                    planning_constraints_blocks(),
                )
            }
        }
        PlanningStep::Constraints => {
            let lowered = text.trim().to_lowercase();
            if is_skip_text(text) || lowered == "none" || lowered == "no constraints" {
                interview.constraints = Some("none".to_string());
            } else {
                interview.constraints = Some(text.to_string());
            }
            interview.step = PlanningStep::Output;
            PlanningAction::PromptWithBlocks(
                "What format do you want the plan in?".to_string(),
                planning_output_blocks(),
            )
        }
        PlanningStep::Output => {
            interview.output = Some(if is_skip_text(text) {
                "checklist".to_string()
            } else {
                text.to_string()
            });
            interview.step = PlanningStep::Summary;
            PlanningAction::PromptWithBlocks(
                planning_summary_mrkdwn(interview),
                planning_confirm_blocks(),
            )
        }
        PlanningStep::Summary => {
            if is_confirm_text(text) {
                let prompt = planning_build_prompt(interview);
                interview.step = PlanningStep::Refining;
                PlanningAction::Dispatch(prompt)
            } else if is_edit_text(text) {
                interview.step = PlanningStep::Editing;
                PlanningAction::Prompt(
                    "Send corrections using lines like:\n`Goal: ...`\n`Constraints: ...`\n`Timeline: ...`\n`Scope: ...`\n`Depth: ...`\n`Output: ...`".to_string(),
                )
            } else if apply_planning_edits(interview, text) {
                PlanningAction::PromptWithBlocks(
                    planning_summary_mrkdwn(interview),
                    planning_confirm_blocks(),
                )
            } else {
                PlanningAction::PromptWithBlocks(
                    "Reply `confirm` to proceed, or send edits like `Goal: ...`.".to_string(),
                    planning_confirm_blocks(),
                )
            }
        }
        PlanningStep::Editing => {
            if apply_planning_edits(interview, text) {
                interview.step = PlanningStep::Summary;
                PlanningAction::PromptWithBlocks(
                    planning_summary_mrkdwn(interview),
                    planning_confirm_blocks(),
                )
            } else {
                PlanningAction::Prompt(
                    "I couldn't read those edits. Try lines like `Goal: ...` or `Constraints: ...`.".to_string(),
                )
            }
        }
        PlanningStep::Refining => {
            if is_done_text(text) {
                *remove_chat = true;
                PlanningAction::Done
            } else {
                PlanningAction::Dispatch(text.to_string())
            }
        }
    }
}

async fn planning_action_for_message(
    state: &SlackState,
    channel: &str,
    thread_ts: Option<&SlackTs>,
    text: &str,
) -> PlanningAction {
    let key = thread_key(channel, thread_ts);
    let now = tokio::time::Instant::now();
    let mut planning = state.planning.lock().await;
    if let Some(interview) = planning.get_mut(&key) {
        interview.last_updated = now;
        if is_cancel_text(text) {
            planning.remove(&key);
            PlanningAction::Cancelled
        } else {
            let mut remove = false;
            let action = planning_advance_step(interview, text, &mut remove);
            if remove {
                planning.remove(&key);
            }
            action
        }
    } else if state.config.planning_auto && should_start_planning_interview(text) {
        let mut interview = PlanningInterview::new(now);
        if !is_bare_plan_request(text) {
            interview.goal = Some(text.to_string());
            interview.step = PlanningStep::Constraints;
            planning.insert(key, interview);
            PlanningAction::PromptWithBlocks(
                "Got it. A couple quick questions to sharpen the plan.\n\nAny constraints?".to_string(),
                planning_constraints_blocks(),
            )
        } else {
            planning.insert(key, interview);
            PlanningAction::Prompt(
                "*Quick planning interview*\n\nWhat does success look like? One sentence is fine.".to_string(),
            )
        }
    } else {
        PlanningAction::None
    }
}

// ── Slash command handlers ──────────────────────────────────────────

async fn command_help(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let help = "*Sparks*\n\n\
        *Commands:*\n\
        `/sparks help` — Show this help\n\
        `/sparks plan [goal]` — Start a planning interview\n\
        `/sparks implement <goal>` — Implement with CLI tool\n\
        `/sparks status` — System status & uptime\n\
        `/sparks model [name]` — Show/switch LLM model\n\
        `/sparks models` — List available models\n\
        `/sparks ghosts` — List active ghosts\n\
        `/sparks memories [query]` — List saved memories\n\
        `/sparks dispatch <ghost> <goal>` — Run an autonomous task\n\
        `/sparks review [summary|detailed] [hours]` — Review session activity\n\
        `/sparks explain [summary|detailed] [hours]` — Conceptual explanation\n\
        `/sparks watch [seconds]` — Real-time activity stream\n\
        `/sparks search <query>` — Search across all sessions\n\
        `/sparks alerts` — Manage alert rules\n\
        `/sparks knobs` — Display all runtime knobs\n\
        `/sparks mood` — Detailed mood state\n\
        `/sparks jobs` — List scheduled cron jobs\n\
        `/sparks session` — Current session info\n\
        `/sparks cli` — Switch CLI tool\n\
        `/sparks set <key> <value>` — Modify runtime knob\n\
        `/sparks cli_model [name]` — Show/switch CLI model\n\n\
        Send any message to chat with Sparks.";
    send_mrkdwn(session, channel, thread_ts, help).await
}

async fn command_status(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let info = &state.system_info;
    let uptime_secs = info.started_at.elapsed().as_secs();
    let ghost_count = state.handle.list_ghosts().len();
    let mood_desc = state.handle.mood.describe();
    let idle = state.handle.activity.idle_secs();
    let last_active = if idle < 5 {
        "just now".to_string()
    } else {
        format!("{} ago", format_duration(idle))
    };
    let current_model = state.handle.llm.current_model();
    let context_window = state.handle.llm.context_window();
    let ctx_label = if context_window >= 1_000_000 {
        format!("{}M", context_window / 1_000_000)
    } else {
        format!("{}k", context_window / 1_000)
    };

    let credits_line = {
        match state.handle.llm.credits().await {
            Ok(Some((total, used))) => {
                let remaining = total - used;
                format!(
                    "\n*Credits:* `${:.2} remaining` (${:.2} used of ${:.2})",
                    remaining, used, total
                )
            }
            _ => String::new(),
        }
    };

    let text = format!(
        "*System Status*\n\n\
         *Provider:* `{}`\n\
         *Model:* `{}`\n\
         *Context:* `{} tokens`\n\
         *Temperature:* `{}`\n\
         *Max tokens:* `{}`\n\
         *Uptime:* `{}`\n\
         *Last active:* `{}`\n\
         *Ghosts:* `{}`\n\
         *Mood:* {}{}",
        escape_mrkdwn(&info.provider),
        escape_mrkdwn(&current_model),
        ctx_label,
        info.temperature,
        info.max_tokens,
        format_duration(uptime_secs),
        escape_mrkdwn(&last_active),
        ghost_count,
        escape_mrkdwn(&mood_desc),
        credits_line,
    );
    send_mrkdwn(session, channel, thread_ts, &text).await
}

async fn command_ghosts(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ghosts = state.handle.list_ghosts();
    let mut out = String::from("*Active ghosts:*\n\n");
    for g in &ghosts {
        out.push_str(&format!(
            "• *{}* — {} `[{}]`\n",
            escape_mrkdwn(&g.name),
            escape_mrkdwn(&g.description),
            escape_mrkdwn(&g.tools.join(", "))
        ));
    }
    send_mrkdwn(session, channel, thread_ts, &out).await
}

async fn command_memories(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match state.handle.list_memories() {
        Ok(memories) if memories.is_empty() => {
            send_mrkdwn(session, channel, thread_ts, "_No memories._").await
        }
        Ok(memories) => {
            let mut out = String::from("*Memories:*\n\n");
            for m in &memories {
                out.push_str(&format!(
                    "`[{}]` *{}* — {}\n",
                    escape_mrkdwn(m.id.get(..8).unwrap_or(&m.id)),
                    escape_mrkdwn(&m.category),
                    escape_mrkdwn(&m.content)
                ));
            }
            send_mrkdwn(session, channel, thread_ts, &out).await
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list memories");
            send_mrkdwn(session, channel, thread_ts, "_An internal error occurred._").await
        }
    }
}

async fn command_model(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if arg.is_empty() {
        let current = state.handle.llm.current_model();
        let text = format!("*Current model:* `{}`", escape_mrkdwn(&current));
        return send_mrkdwn(session, channel, thread_ts, &text).await;
    }
    if arg == "reset" {
        state.handle.llm.set_model_override(None);
        let current = state.handle.llm.current_model();
        let text = format!("*Reset to default:* `{}`", escape_mrkdwn(&current));
        return send_mrkdwn(session, channel, thread_ts, &text).await;
    }
    state.handle.llm.set_model_override(Some(arg.to_string()));
    let text = format!("*Model set to:* `{}`", escape_mrkdwn(arg));
    send_mrkdwn(session, channel, thread_ts, &text).await
}

async fn command_models(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match state.handle.llm.list_models().await {
        Ok(models) if models.is_empty() => {
            send_mrkdwn(session, channel, thread_ts, "_No models returned by API._").await
        }
        Ok(models) => {
            let current = state.handle.llm.current_model();
            let mut out = String::from("*Available models:*\n\n");
            for m in &models {
                if *m == current {
                    out.push_str(&format!("• *{}* (active)\n", escape_mrkdwn(m)));
                } else {
                    out.push_str(&format!("• `{}`\n", escape_mrkdwn(m)));
                }
            }
            send_mrkdwn(session, channel, thread_ts, &out).await
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list models");
            send_mrkdwn(session, channel, thread_ts, "_Failed to list models._").await
        }
    }
}

async fn command_dispatch(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if arg.is_empty() {
        return send_mrkdwn(
            session,
            channel,
            thread_ts,
            "_Usage: /sparks dispatch <ghost> <goal>_",
        )
        .await;
    }

    let (ghost_name, goal) = match arg.split_once(' ') {
        Some((g, goal)) => (Some(g.to_string()), goal.to_string()),
        None => (None, arg.to_string()),
    };

    let target = crate::pulse::PulseTarget::Session(SessionContext {
        platform: "slack".into(),
        user_id: user_id.to_string(),
        chat_id: channel.to_string(),
    });
    let task = crate::core::AutonomousTask {
        goal,
        context: String::new(),
        ghost: ghost_name.clone(),
        target,
        lane: "delivery".to_string(),
        risk_tier: "medium".to_string(),
        repo: crate::kpi::default_repo_name(),
        task_id: None,
    };

    match state.handle.dispatch_task(task).await {
        Err(e) => {
            tracing::error!(error = %e, "Failed to dispatch task");
            send_mrkdwn(session, channel, thread_ts, "_Failed to dispatch task._").await
        }
        Ok(task_id) => {
            let label = ghost_name.unwrap_or_else(|| "auto".into());
            let text = format!(
                "_:zap: Dispatched to {} (task_id={})_",
                escape_mrkdwn(&label),
                escape_mrkdwn(&task_id)
            );
            send_mrkdwn(session, channel, thread_ts, &text).await
        }
    }
}

async fn command_review(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::session_review::{render_review_mrkdwn, ReviewDetail};

    let args: Vec<&str> = arg.split_whitespace().collect();
    let detail = args
        .first()
        .map(|a| ReviewDetail::from_str_loose(a))
        .unwrap_or(ReviewDetail::Standard);
    let hours: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(24);

    let session_key = format!("slack:{}:{}", user_id, channel);
    let entries = match state.handle.activity_log.recent(&session_key, 200) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load activity log");
            return send_mrkdwn(session, channel, thread_ts, "_Failed to load activity log._").await;
        }
    };
    let auto_entries = state
        .handle
        .activity_log
        .recent("autonomous", 100)
        .unwrap_or_default();
    let mut all_entries = entries;
    all_entries.extend(auto_entries);
    all_entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
    let filtered: Vec<_> = all_entries
        .into_iter()
        .filter(|e| e.created_at >= cutoff_str)
        .collect();

    let review = render_review_mrkdwn(&filtered, detail);
    send_mrkdwn(session, channel, thread_ts, &review).await
}

async fn command_explain(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::session_review::{generate_explanation, ReviewDetail};

    let args: Vec<&str> = arg.split_whitespace().collect();
    let detail = args
        .first()
        .map(|a| ReviewDetail::from_str_loose(a))
        .unwrap_or(ReviewDetail::Standard);
    let hours: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(24);

    let session_key = format!("slack:{}:{}", user_id, channel);
    let _ = send_mrkdwn(session, channel, thread_ts, "_Generating explanation..._").await;

    let entries = state
        .handle
        .activity_log
        .recent(&session_key, 200)
        .unwrap_or_default();
    let auto_entries = state
        .handle
        .activity_log
        .recent("autonomous", 100)
        .unwrap_or_default();
    let mut all_entries = entries;
    all_entries.extend(auto_entries);
    all_entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
    let filtered: Vec<_> = all_entries
        .into_iter()
        .filter(|e| e.created_at >= cutoff_str)
        .collect();

    match generate_explanation(&filtered, state.handle.llm.as_ref(), detail).await {
        Ok(explanation) => {
            let text = format!("*Session Explanation*\n\n{}", escape_mrkdwn(&explanation));
            send_mrkdwn(session, channel, thread_ts, &text).await
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to generate explanation");
            send_mrkdwn(session, channel, thread_ts, "_Failed to generate explanation._").await
        }
    }
}

async fn command_search(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::session_review::render_search_results_mrkdwn;

    if arg.is_empty() {
        return send_mrkdwn(
            session,
            channel,
            thread_ts,
            "*Usage:* `/sparks search <query>`\n\nSearch across all sessions.",
        )
        .await;
    }

    let entries = state
        .handle
        .activity_log
        .search(arg, 50)
        .unwrap_or_default();
    let mrkdwn = render_search_results_mrkdwn(&entries, arg);
    send_mrkdwn(session, channel, thread_ts, &mrkdwn).await
}

async fn command_alerts(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<&str> = arg.splitn(5, ' ').collect();
    let subcommand = args.first().copied().unwrap_or("");

    match subcommand {
        "add" => {
            if args.len() < 3 {
                return send_mrkdwn(
                    session,
                    channel,
                    thread_ts,
                    "*Usage:* `/sparks alerts add <name> <pattern> [target] [severity]`",
                )
                .await;
            }
            let name = args[1];
            let pattern = args[2];
            let target = args.get(3).copied().unwrap_or("tool_name");
            let severity = args.get(4).copied().unwrap_or("warn");
            let chat_str = channel.to_string();

            match state
                .handle
                .activity_log
                .add_alert_rule(name, pattern, target, severity, Some(&chat_str))
            {
                Ok(id) => {
                    let text = format!(
                        ":white_check_mark: Alert rule *#{}* created: `{}` on _{}_  [{}]",
                        id, pattern, target, severity
                    );
                    send_mrkdwn(session, channel, thread_ts, &text).await
                }
                Err(e) => {
                    let text = {
                        tracing::error!(error = %e, "Alert operation failed");
                        "_An error occurred._".to_string()
                    };
                    send_mrkdwn(session, channel, thread_ts, &text).await
                }
            }
        }
        "remove" | "rm" | "delete" => {
            let id: i64 = match args.get(1).and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => {
                    return send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        "*Usage:* `/sparks alerts remove <id>`",
                    )
                    .await;
                }
            };
            match state.handle.activity_log.remove_alert_rule(id) {
                Ok(true) => {
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &format!(":white_check_mark: Alert rule #{} removed.", id),
                    )
                    .await
                }
                Ok(false) => {
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &format!(":warning: Alert rule #{} not found.", id),
                    )
                    .await
                }
                Err(e) => {
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &{
                        tracing::error!(error = %e, "Alert operation failed");
                        "_An error occurred._".to_string()
                    },
                    )
                    .await
                }
            }
        }
        "toggle" => {
            let id: i64 = match args.get(1).and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => {
                    return send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        "*Usage:* `/sparks alerts toggle <id>`",
                    )
                    .await;
                }
            };
            let rules = state
                .handle
                .activity_log
                .list_alert_rules()
                .unwrap_or_default();
            let current = rules.iter().find(|r| r.id == id);
            let new_state = current.map(|r| !r.enabled).unwrap_or(true);
            match state.handle.activity_log.toggle_alert_rule(id, new_state) {
                Ok(true) => {
                    let label = if new_state { "enabled" } else { "disabled" };
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &format!(":white_check_mark: Alert rule #{} {}.", id, label),
                    )
                    .await
                }
                Ok(false) => {
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &format!(":warning: Alert rule #{} not found.", id),
                    )
                    .await
                }
                Err(e) => {
                    send_mrkdwn(
                        session,
                        channel,
                        thread_ts,
                        &{
                        tracing::error!(error = %e, "Alert operation failed");
                        "_An error occurred._".to_string()
                    },
                    )
                    .await
                }
            }
        }
        _ => {
            use crate::session_review::render_alert_rules_mrkdwn;
            let rules = state
                .handle
                .activity_log
                .list_alert_rules()
                .unwrap_or_default();
            let mrkdwn = render_alert_rules_mrkdwn(&rules);
            send_mrkdwn(session, channel, thread_ts, &mrkdwn).await
        }
    }
}

async fn command_watch(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let duration_secs: u64 = arg
        .split_whitespace()
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(300)
        .min(3600);

    let session_key = format!("slack:{}:{}", user_id, channel);
    send_mrkdwn(
        session,
        channel,
        thread_ts,
        &format!(
            ":eyes: *Watch mode active* for {} seconds.\nNew activity will be streamed here.",
            duration_secs
        ),
    )
    .await?;

    let client = state.client.clone();
    let bot_token = state.bot_token.clone();
    let activity_log = state.handle.activity_log.clone();
    let channel = channel.clone();
    let thread_ts = thread_ts.cloned();
    tokio::spawn(async move {
        run_watch_loop(
            &client,
            &bot_token,
            &channel,
            thread_ts.as_ref(),
            &session_key,
            duration_secs,
            activity_log,
        )
        .await;
    });
    Ok(())
}

async fn run_watch_loop(
    client: &SlackHyperClient,
    bot_token: &SlackApiToken,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    session_key: &str,
    duration_secs: u64,
    activity_log: Arc<ActivityLogStore>,
) {
    let mut last_id = latest_entry_id(&activity_log, session_key);
    let mut last_auto_id = latest_entry_id(&activity_log, "autonomous");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(duration_secs);

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let session = client.open_session(&bot_token);
        let new_entries = activity_log.recent(session_key, 20).unwrap_or_default();
        for entry in &new_entries {
            if entry.id > last_id {
                last_id = entry.id;
                let text = format_session_entry(entry);
                let _ = send_mrkdwn(&session, channel, thread_ts, &text).await;
            }
        }
        let auto_entries = activity_log.recent("autonomous", 20).unwrap_or_default();
        for entry in &auto_entries {
            if entry.id > last_auto_id {
                last_auto_id = entry.id;
                let text = format_autonomous_entry(entry);
                let _ = send_mrkdwn(&session, channel, thread_ts, &text).await;
            }
        }
    }

    let session = client.open_session(&bot_token);
    let _ = send_mrkdwn(&session, channel, thread_ts, ":eyes: Watch mode ended.").await;
}

fn latest_entry_id(activity_log: &ActivityLogStore, session_key: &str) -> i64 {
    activity_log
        .recent(session_key, 1)
        .ok()
        .and_then(|e| e.last().map(|entry| entry.id))
        .unwrap_or(0)
}

fn format_session_entry(entry: &ActivityEntry) -> String {
    let emoji = match entry.event_type.as_str() {
        "chat_in" => ":speech_balloon:",
        "chat_out" => ":robot_face:",
        "tool_run" => ":wrench:",
        "task_start" => ":rocket:",
        "task_finish" => ":white_check_mark:",
        "task_fail" => ":x:",
        _ => ":small_blue_diamond:",
    };
    let mut text = format!("{} {}", emoji, escape_mrkdwn(&entry.summary));
    if let Some(ref tool) = entry.tool_name {
        text.push_str(&format!(" [{}]", escape_mrkdwn(tool)));
    }
    if let Some(ms) = entry.duration_ms {
        text.push_str(&format!(" _({}ms)_", ms));
    }
    if let Some(ref input) = entry.tool_input {
        let preview = if input.len() > 100 {
            &input[..input.floor_char_boundary(100)]
        } else {
            input
        };
        text.push_str(&format!("\n`{}`", escape_mrkdwn(preview)));
    }
    text
}

fn format_autonomous_entry(entry: &ActivityEntry) -> String {
    let emoji = match entry.event_type.as_str() {
        "task_start" => ":rocket:",
        "task_finish" => ":white_check_mark:",
        "task_fail" => ":x:",
        "tool_run" => ":wrench:",
        _ => ":zap:",
    };
    let mut text = format!("[auto] {} {}", emoji, escape_mrkdwn(&entry.summary));
    if let Some(ref ghost) = entry.ghost {
        text.push_str(&format!(" [{}]", escape_mrkdwn(ghost)));
    }
    if let Some(ms) = entry.duration_ms {
        text.push_str(&format!(" _({}ms)_", ms));
    }
    text
}

async fn command_knobs(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let display = {
        let k = state
            .handle
            .knobs
            .read()
            .unwrap_or_else(|e| e.into_inner());
        k.display()
    };
    let text = format!("```{}```", escape_mrkdwn(&display));
    send_mrkdwn(session, channel, thread_ts, &text).await
}

async fn command_mood(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let desc = state.handle.mood.describe();
    let energy = state.handle.mood.energy();
    let modifier = state.handle.mood.modifier();
    let text = format!(
        "*Mood*\n\n{}\n*Energy:* `{} {:.0}%`\n*Modifier:* `{}`",
        escape_mrkdwn(&desc),
        energy_bar(energy),
        energy * 100.0,
        escape_mrkdwn(&modifier),
    );
    send_mrkdwn(session, channel, thread_ts, &text).await
}

async fn command_jobs(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(engine) = &state.handle.cron_engine {
        match engine.list_jobs() {
            Ok(jobs) if jobs.is_empty() => {
                send_mrkdwn(session, channel, thread_ts, "_No scheduled jobs._").await
            }
            Ok(jobs) => {
                let mut out = String::from("*Scheduled Jobs:*\n\n");
                for j in &jobs {
                    let status = if j.enabled { "on" } else { "off" };
                    let next = j
                        .next_run
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".to_string());
                    out.push_str(&format!(
                        "`[{}]` *{}* ({})\n  next: {} — {}\n",
                        escape_mrkdwn(j.id.get(..8).unwrap_or(&j.id)),
                        escape_mrkdwn(&j.name),
                        status,
                        escape_mrkdwn(&next),
                        escape_mrkdwn(&j.prompt),
                    ));
                    if let Some(ghost) = j.ghost.as_deref() {
                        out.push_str(&format!("  ghost: {}\n", escape_mrkdwn(ghost)));
                    }
                    if j.target != "broadcast" {
                        out.push_str(&format!("  target: {}\n", escape_mrkdwn(&j.target)));
                    }
                }
                send_mrkdwn(session, channel, thread_ts, &out).await
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to list jobs");
                send_mrkdwn(session, channel, thread_ts, "_Failed to list jobs._").await
            }
        }
    } else {
        send_mrkdwn(
            session,
            channel,
            thread_ts,
            "_Cron engine not initialized._",
        )
        .await
    }
}

async fn command_session(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    channel_str: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_key = format!("slack:{}:{}", user_id, channel_str);
    let turns = state
        .handle
        .memory
        .recent_turns(&session_key, 100)
        .unwrap_or_default();

    let turn_count = turns.len();
    let total_chars: usize = turns.iter().map(|(_, c)| c.len()).sum();
    let est_tokens = total_chars / 4;
    let context_window = state.handle.llm.context_window();
    let utilization = if context_window > 0 {
        (est_tokens as f64 / context_window as f64 * 100.0).min(100.0)
    } else {
        0.0
    };
    let current_model = state.handle.llm.current_model();

    let last_preview = turns
        .last()
        .map(|(role, content)| {
            let preview = if content.len() > 80 {
                format!("{}...", &content[..content.floor_char_boundary(80)])
            } else {
                content.clone()
            };
            format!("[{}] {}", role, preview)
        })
        .unwrap_or_else(|| "none".to_string());

    let text = format!(
        "*Session*\n\n\
         *Key:* `{}`\n\
         *Model:* `{}`\n\
         *Turns:* `{}`\n\
         *Est. tokens:* `~{}`\n\
         *Context:* `{:.0}%` of `{}`\n\
         *Last message:*\n_{}_",
        escape_mrkdwn(&session_key),
        escape_mrkdwn(&current_model),
        turn_count,
        format_tokens(est_tokens as u64),
        utilization,
        format_tokens(context_window),
        escape_mrkdwn(&last_preview),
    );
    send_mrkdwn(session, channel, thread_ts, &text).await
}

async fn command_cli(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let current = state
        .handle
        .knobs
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .cli_tool
        .clone();
    let label = cli_display_name(&current);
    let blocks = vec![
        SlackBlock::Section(
            SlackSectionBlock::new().with_text(SlackBlockText::MarkDown(
                SlackBlockMarkDownText::new(format!("*Coding CLI tool:* {}", escape_mrkdwn(&label))),
            )),
        ),
        SlackBlock::Actions(SlackActionsBlock::new(
            CLI_TOOLS
                .iter()
                .map(|(id, name)| {
                    SlackActionBlockElement::Button(
                        SlackBlockButtonElement::new(
                            format!("cli:{}", id).into(),
                            pt(name.to_string()),
                        )
                        .with_value(id.to_string().into()),
                    )
                })
                .collect(),
        )),
    ];
    send_mrkdwn_with_blocks(
        session,
        channel,
        thread_ts,
        &format!("*Coding CLI tool:* {}", escape_mrkdwn(&label)),
        blocks,
    )
    .await
}

async fn command_set(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.is_empty() {
        return command_knobs(session, channel, thread_ts, state).await;
    }
    if parts.len() == 2 {
        let result = {
            let mut k = state
                .handle
                .knobs
                .write()
                .unwrap_or_else(|e| e.into_inner());
            k.set(parts[0], parts[1])
        };
        match result {
            Ok(msg) => {
                state
                    .handle
                    .observer
                    .emit(crate::observer::ObserverEvent::new(
                        crate::observer::ObserverCategory::KnobChange,
                        format!("{} = {}", parts[0], parts[1]),
                    ));
                let text = format!("*Set:* {}", escape_mrkdwn(&msg));
                send_mrkdwn(session, channel, thread_ts, &text).await
            }
            Err(e) => {
                let text = format!("_Error: {}_", escape_mrkdwn(&e));
                send_mrkdwn(session, channel, thread_ts, &text).await
            }
        }
    } else {
        send_mrkdwn(
            session,
            channel,
            thread_ts,
            "_Usage:_ `/sparks set` _or_ `/sparks set <key> <value>`",
        )
        .await
    }
}

async fn command_cli_model(
    session: &SlackClientSession<'_, SlackClientHyperHttpsConnector>,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    arg: &str,
    state: &SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if arg.is_empty() {
        let model = state
            .handle
            .knobs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .cli_model
            .clone();
        let label = if model.is_empty() {
            "default (tool decides)".to_string()
        } else {
            model
        };
        let text = format!("*CLI tool model:* `{}`", escape_mrkdwn(&label));
        send_mrkdwn(session, channel, thread_ts, &text).await
    } else {
        let result = {
            let mut k = state
                .handle
                .knobs
                .write()
                .unwrap_or_else(|e| e.into_inner());
            k.set("cli_model", arg)
        };
        match result {
            Ok(msg) => {
                let text = format!("*Set:* {}", escape_mrkdwn(&msg));
                send_mrkdwn(session, channel, thread_ts, &text).await
            }
            Err(e) => {
                let text = format!("_Error: {}_", escape_mrkdwn(&e));
                send_mrkdwn(session, channel, thread_ts, &text).await
            }
        }
    }
}

// ── Slash command dispatch ──────────────────────────────────────────

async fn handle_slash_command(
    text: &str,
    channel: SlackChannelId,
    user_id: SlackUserId,
    state: SlackState,
) {
    let session = state.client.open_session(&state.bot_token);
    let channel_str = channel.to_string();
    let user_str = user_id.to_string();

    if !is_authorized(&channel_str, &state.config) {
        tracing::debug!(channel = %channel_str, "Ignoring slash command from unauthorized channel");
        return;
    }

    // Rate limit slash commands (except help/status which are read-only)
    {
        let rate_key = format!("{}:{}", channel_str, user_str);
        let now = tokio::time::Instant::now();
        let mut map = state.last_request.lock().await;
        if should_rate_limit(
            map.get(&rate_key).copied(),
            now,
            tokio::time::Duration::from_secs(5),
        ) {
            return;
        }
        map.insert(rate_key, now);
    }

    // Parse subcommand and args
    let trimmed = text.trim();
    let (subcmd, arg) = match trimmed.split_once(' ') {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };

    let result: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> = match subcmd {
        "help" | "" => command_help(&session, &channel, None).await,
        "status" => command_status(&session, &channel, None, &state).await,
        "ghosts" => command_ghosts(&session, &channel, None, &state).await,
        "memories" => command_memories(&session, &channel, None, &state).await,
        "model" => command_model(&session, &channel, None, arg, &state).await,
        "models" => command_models(&session, &channel, None, &state).await,
        "dispatch" => {
            command_dispatch(&session, &channel, None, &user_str, arg, &state).await
        }
        "review" => {
            command_review(&session, &channel, None, &user_str, arg, &state).await
        }
        "explain" => {
            command_explain(&session, &channel, None, &user_str, arg, &state).await
        }
        "search" => command_search(&session, &channel, None, arg, &state).await,
        "alerts" => command_alerts(&session, &channel, None, arg, &state).await,
        "watch" => {
            command_watch(&session, &channel, None, &user_str, arg, &state).await
        }
        "knobs" => command_knobs(&session, &channel, None, &state).await,
        "mood" => command_mood(&session, &channel, None, &state).await,
        "jobs" => command_jobs(&session, &channel, None, &state).await,
        "session" => {
            command_session(&session, &channel, None, &user_str, &channel_str, &state).await
        }
        "cli" => command_cli(&session, &channel, None, &state).await,
        "set" => command_set(&session, &channel, None, arg, &state).await,
        "cli_model" => command_cli_model(&session, &channel, None, arg, &state).await,
        "plan" => {
            if !state.config.planning_enabled {
                send_mrkdwn(&session, &channel, None, "_Planning interview is disabled in config._")
                    .await
            } else if arg == "cancel" || arg == "stop" {
                let key = thread_key(&channel_str, None);
                state.planning.lock().await.remove(&key);
                send_mrkdwn(&session, &channel, None, "_Planning cancelled._").await
            } else {
                let now = tokio::time::Instant::now();
                let mut interview = PlanningInterview::new(now);
                let key = thread_key(&channel_str, None);
                if arg.is_empty() {
                    state.planning.lock().await.insert(key, interview);
                    send_mrkdwn(
                        &session,
                        &channel,
                        None,
                        "*Quick planning interview*\n\nWhat does success look like? One sentence is fine.",
                    )
                    .await
                } else {
                    interview.goal = Some(arg.to_string());
                    interview.step = PlanningStep::Constraints;
                    state.planning.lock().await.insert(key, interview);
                    send_mrkdwn_with_blocks(
                        &session,
                        &channel,
                        None,
                        "Got it. A couple quick questions to sharpen the plan.\n\nAny constraints?",
                        planning_constraints_blocks(),
                    )
                    .await
                }
            }
        }
        "implement" => {
            if arg.is_empty() {
                send_mrkdwn(
                    &session,
                    &channel,
                    None,
                    "*Usage:* `/sparks implement <goal>`\n\nDescribe what you want to implement.",
                )
                .await
            } else {
                let prompt = build_implement_prompt(arg, None);
                let session_ctx = slack_session(&user_str, &channel_str);
                dispatch_to_core(
                    &state,
                    channel.clone(),
                    None,
                    session_ctx,
                    prompt,
                    "_Status: Implementing..._",
                )
                .await
            }
        }
        _ => {
            // Treat as a chat message
            let session_ctx = slack_session(&user_str, &channel_str);
            dispatch_to_core(
                &state,
                channel,
                None,
                session_ctx,
                text.to_string(),
                "_Status: Starting..._",
            )
            .await
        }
    };

    if let Err(e) = result {
        tracing::error!(error = %e, "Slack command handler error");
    }
}

// ── Push event handler ──────────────────────────────────────────────

async fn handle_push_event(
    event: SlackPushEventCallback,
    state: SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match event.event {
        SlackEventCallbackBody::Message(msg_event) => {
            handle_slack_message(msg_event, state).await?;
        }
        SlackEventCallbackBody::AppMention(mention_event) => {
            handle_app_mention(mention_event, state).await?;
        }
        _ => {} // Ignore other events
    }
    Ok(())
}

async fn handle_slack_message(
    msg: SlackMessageEvent,
    state: SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Ignore bot messages (including our own)
    if msg.sender.bot_id.is_some() {
        return Ok(());
    }
    // Ignore subtypes (edits, deletes, etc.)
    if msg.subtype.is_some() {
        return Ok(());
    }

    let Some(channel_id) = &msg.origin.channel else {
        return Ok(());
    };
    let channel_str = channel_id.to_string();

    // Auth check
    if !is_authorized(&channel_str, &state.config) {
        return Ok(());
    }

    let user_id = msg
        .sender
        .user
        .as_ref()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "unknown".into());

    let text = msg
        .content
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();

    if text.trim().is_empty() {
        return Ok(());
    }

    let ts = &msg.origin.ts;
    // Use the message's ts as thread_ts for replies (if thread_replies is true)
    let thread_ts = if state.config.thread_replies {
        Some(
            msg.origin
                .thread_ts
                .clone()
                .unwrap_or_else(|| ts.clone()),
        )
    } else {
        msg.origin.thread_ts.clone()
    };

    // Rate limit check
    {
        let rate_key = format!("{}:{}", channel_str, user_id);
        let mut last_req = state.last_request.lock().await;
        let now = tokio::time::Instant::now();
        if should_rate_limit(
            last_req.get(&rate_key).copied(),
            now,
            tokio::time::Duration::from_secs(5),
        ) {
            return Ok(());
        }
        last_req.insert(rate_key, now);
    }

    // Planning check
    if state.config.planning_enabled {
        let action =
            planning_action_for_message(&state, &channel_str, thread_ts.as_ref(), &text).await;
        match action {
            PlanningAction::None => {}
            PlanningAction::Cancelled => {
                let session = state.client.open_session(&state.bot_token);
                let _ = send_mrkdwn(
                    &session,
                    channel_id,
                    thread_ts.as_ref(),
                    "_Planning cancelled._",
                )
                .await;
                return Ok(());
            }
            PlanningAction::Prompt(prompt) => {
                let session = state.client.open_session(&state.bot_token);
                let _ = send_mrkdwn(&session, channel_id, thread_ts.as_ref(), &prompt).await;
                return Ok(());
            }
            PlanningAction::PromptWithBlocks(prompt, blocks) => {
                let session = state.client.open_session(&state.bot_token);
                let _ = send_mrkdwn_with_blocks(
                    &session,
                    channel_id,
                    thread_ts.as_ref(),
                    &prompt,
                    blocks,
                )
                .await;
                return Ok(());
            }
            PlanningAction::Dispatch(prompt) => {
                let session_ctx = slack_session(&user_id, &channel_str);
                dispatch_to_core_with_followup(
                    &state,
                    channel_id.clone(),
                    thread_ts,
                    session_ctx,
                    prompt,
                    "_Status: Drafting plan..._",
                    Some("*Plan generated.* What would you like to do next?".to_string()),
                )
                .await?;
                return Ok(());
            }
            PlanningAction::Done => {
                let session = state.client.open_session(&state.bot_token);
                let _ = send_mrkdwn(
                    &session,
                    channel_id,
                    thread_ts.as_ref(),
                    "_Plan finalised._",
                )
                .await;
                return Ok(());
            }
        }
    }

    // Regular chat dispatch
    let session_ctx = slack_session(&user_id, &channel_str);
    dispatch_to_core(
        &state,
        channel_id.clone(),
        thread_ts,
        session_ctx,
        text,
        "_Status: Starting..._",
    )
    .await
}

async fn handle_app_mention(
    mention: SlackAppMentionEvent,
    state: SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let channel_str = mention.channel.to_string();

    if !is_authorized(&channel_str, &state.config) {
        return Ok(());
    }

    let user_id = mention.user.to_string();

    // Rate limit mentions
    {
        let rate_key = format!("{}:{}", channel_str, user_id);
        let now = tokio::time::Instant::now();
        let mut map = state.last_request.lock().await;
        if should_rate_limit(
            map.get(&rate_key).copied(),
            now,
            tokio::time::Duration::from_secs(5),
        ) {
            return Ok(());
        }
        map.insert(rate_key, now);
    }
    let text = mention
        .content
        .text
        .clone()
        .unwrap_or_default();

    if text.trim().is_empty() {
        return Ok(());
    }

    // Strip the @mention from the text
    let cleaned_text = text
        .split_whitespace()
        .skip_while(|w| w.starts_with("<@"))
        .collect::<Vec<_>>()
        .join(" ");

    if cleaned_text.trim().is_empty() {
        return Ok(());
    }

    let ts = &mention.origin.ts;
    let thread_ts = if state.config.thread_replies {
        Some(
            mention
                .origin
                .thread_ts
                .clone()
                .unwrap_or_else(|| ts.clone()),
        )
    } else {
        mention.origin.thread_ts.clone()
    };

    let session_ctx = slack_session(&user_id, &channel_str);
    dispatch_to_core(
        &state,
        mention.channel.clone(),
        thread_ts,
        session_ctx,
        cleaned_text,
        "_Status: Starting..._",
    )
    .await
}

// ── Interaction handler ─────────────────────────────────────────────

async fn handle_interaction_event(
    event: SlackInteractionEvent,
    state: SlackState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let SlackInteractionEvent::BlockActions(action_event) = event {
        let channel_id = action_event
            .channel
            .as_ref()
            .map(|c| c.id.clone());
        let user_id = action_event
            .user
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_else(|| "unknown".into());

        // Authorization check: reject interactions from unauthorized channels
        if let Some(ch) = &channel_id {
            if !is_authorized(&ch.to_string(), &state.config) {
                tracing::debug!(channel = %ch, "Ignoring interaction from unauthorized channel");
                return Ok(());
            }
        }

        if let Some(actions) = &action_event.actions {
            for action in actions {
                let action_id_str = action.action_id.to_string();
                let parts: Vec<&str> = action_id_str.splitn(3, ':').collect();

                match parts.as_slice() {
                    ["confirm", id, decision] => {
                        let approved = *decision == "yes";
                        let mut pending = state.pending.lock().await;
                        if let Some(tx) = pending.remove(*id) {
                            let _ = tx.send(approved);
                        }
                    }
                    ["plan", kind, value] => {
                        if let Some(ch) = &channel_id {
                            let channel_str = ch.to_string();
                            // Handle quick-select buttons from constraints step
                            match (*kind, *value) {
                                ("timeline", v) | ("scope", v) | ("constraints", v) => {
                                    handle_planning_quick_select(
                                        &state, &channel_str, ch, None, kind, v,
                                    )
                                    .await;
                                }
                                ("output", v) => {
                                    handle_planning_quick_select(
                                        &state, &channel_str, ch, None, "output", v,
                                    )
                                    .await;
                                }
                                ("skip", _) => {
                                    handle_planning_quick_select(
                                        &state, &channel_str, ch, None, "skip", value,
                                    )
                                    .await;
                                }
                                _ => {
                                    handle_planning_callback(
                                        &state,
                                        &channel_str,
                                        ch,
                                        None,
                                        &user_id,
                                        kind,
                                        value,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    ["cli", tool_id] => {
                        {
                            let mut k = state
                                .handle
                                .knobs
                                .write()
                                .unwrap_or_else(|e| e.into_inner());
                            let _ = k.set("cli_tool", tool_id);
                        }
                        if let Some(ch) = &channel_id {
                            let label = cli_display_name(tool_id);
                            let session = state.client.open_session(&state.bot_token);
                            let _ = send_mrkdwn(
                                &session,
                                ch,
                                None,
                                &format!("*CLI tool switched to:* {}", escape_mrkdwn(&label)),
                            )
                            .await;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Handle quick-select button presses from planning constraints/output steps.
async fn handle_planning_quick_select(
    state: &SlackState,
    channel_str: &str,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    kind: &str,
    value: &str,
) {
    let key = thread_key(channel_str, thread_ts);
    let mut planning = state.planning.lock().await;
    let Some(interview) = planning.get_mut(&key) else {
        return;
    };
    interview.last_updated = tokio::time::Instant::now();

    match kind {
        "timeline" => {
            let label = planning_value_label("timeline", value).unwrap_or(value);
            interview.timeline = Some(label.to_string());
            // Stay on constraints step, user can add more or skip
        }
        "scope" => {
            let label = planning_value_label("scope", value).unwrap_or(value);
            interview.scope = Some(label.to_string());
        }
        "constraints" if value == "none" => {
            interview.constraints = Some("none".to_string());
            // Advance to output step
            interview.step = PlanningStep::Output;
            drop(planning);
            let session = state.client.open_session(&state.bot_token);
            let _ = send_mrkdwn_with_blocks(
                &session,
                channel,
                thread_ts,
                "What format do you want the plan in?",
                planning_output_blocks(),
            )
            .await;
            return;
        }
        "skip" => {
            // Skip current step
            match interview.step {
                PlanningStep::Constraints => {
                    interview.constraints = Some("none".to_string());
                    interview.step = PlanningStep::Output;
                    drop(planning);
                    let session = state.client.open_session(&state.bot_token);
                    let _ = send_mrkdwn_with_blocks(
                        &session,
                        channel,
                        thread_ts,
                        "What format do you want the plan in?",
                        planning_output_blocks(),
                    )
                    .await;
                    return;
                }
                _ => {}
            }
        }
        "output" => {
            interview.output = Some(value.to_string());
            interview.step = PlanningStep::Summary;
            let summary = planning_summary_mrkdwn(interview);
            drop(planning);
            let session = state.client.open_session(&state.bot_token);
            let _ = send_mrkdwn_with_blocks(
                &session,
                channel,
                thread_ts,
                &summary,
                planning_confirm_blocks(),
            )
            .await;
            return;
        }
        _ => {}
    }
    drop(planning);

    // For timeline/scope selections, acknowledge and wait for more input or skip
    let session = state.client.open_session(&state.bot_token);
    let label = match kind {
        "timeline" => format!("Timeline: *{}*. ", value),
        "scope" => format!("Scope: *{}*. ", value),
        _ => String::new(),
    };
    let _ = send_mrkdwn(
        &session,
        channel,
        thread_ts,
        &format!("{}Anything else? Type constraints or press *Skip*.", label),
    )
    .await;
}

async fn handle_planning_callback(
    state: &SlackState,
    channel_str: &str,
    channel: &SlackChannelId,
    thread_ts: Option<&SlackTs>,
    user_id: &str,
    kind: &str,
    value: &str,
) {
    let key = thread_key(channel_str, thread_ts);
    let session = state.client.open_session(&state.bot_token);

    // Handle post-generation actions
    if kind == "post" {
        match value {
            "implement" => {
                let interview = { state.planning.lock().await.remove(&key) };
                let goal = interview
                    .as_ref()
                    .and_then(|iv| iv.goal.clone())
                    .unwrap_or_default();
                let prompt = build_implement_prompt(&goal, interview.as_ref());
                let session_ctx = slack_session(user_id, channel_str);
                let _ = dispatch_to_core(
                    state,
                    channel.clone(),
                    thread_ts.cloned(),
                    session_ctx,
                    prompt,
                    "_Status: Implementing..._",
                )
                .await;
            }
            "refine" => {
                let _ = send_mrkdwn(
                    &session,
                    channel,
                    thread_ts,
                    "_Send me what you'd like to change and I'll update the plan._",
                )
                .await;
            }
            "done" => {
                state.planning.lock().await.remove(&key);
                let _ = send_mrkdwn(&session, channel, thread_ts, "_Plan finalised._").await;
            }
            _ => {}
        }
        return;
    }

    // Handle confirm/edit/cancel
    if kind == "confirm" {
        let mut planning = state.planning.lock().await;
        let Some(interview) = planning.get_mut(&key) else {
            return;
        };
        interview.last_updated = tokio::time::Instant::now();
        match value {
            "yes" => {
                let prompt = planning_build_prompt(interview);
                interview.step = PlanningStep::Refining;
                drop(planning);
                let session_ctx = slack_session(user_id, channel_str);
                let _ = dispatch_to_core_with_followup(
                    state,
                    channel.clone(),
                    thread_ts.cloned(),
                    session_ctx,
                    prompt,
                    "_Status: Drafting plan..._",
                    Some("*Plan generated.* What would you like to do next?".to_string()),
                )
                .await;
            }
            "edit" => {
                interview.step = PlanningStep::Editing;
                let _ = send_mrkdwn(
                    &session,
                    channel,
                    thread_ts,
                    "Send corrections using lines like:\n`Goal: ...`\n`Constraints: ...`\n`Timeline: ...`\n`Scope: ...`\n`Depth: ...`\n`Output: ...`",
                )
                .await;
            }
            "cancel" => {
                planning.remove(&key);
                let _ = send_mrkdwn(&session, channel, thread_ts, "_Planning cancelled._").await;
            }
            _ => {}
        }
        return;
    }

    // Handle skip
    if kind == "skip" {
        let mut planning = state.planning.lock().await;
        let Some(interview) = planning.get_mut(&key) else {
            return;
        };
        interview.last_updated = tokio::time::Instant::now();
        match value {
            "constraints" => {
                interview.constraints = Some("none".into());
                interview.step = PlanningStep::Output;
                let _ = send_mrkdwn(
                    &session,
                    channel,
                    thread_ts,
                    "What format do you want the plan in: checklist, spec, or draft?",
                )
                .await;
            }
            "output" => {
                interview.output = Some("checklist".into());
                interview.step = PlanningStep::Summary;
                let summary = planning_summary_mrkdwn(interview);
                let _ = send_mrkdwn(&session, channel, thread_ts, &summary).await;
            }
            _ => {}
        }
        return;
    }

    // Handle value selections (timeline, scope, constraints, output, depth)
    let Some(label) = planning_value_label(kind, value) else {
        return;
    };

    let mut planning = state.planning.lock().await;
    let Some(interview) = planning.get_mut(&key) else {
        return;
    };
    interview.last_updated = tokio::time::Instant::now();

    match kind {
        "depth" => interview.depth = Some(label.into()),
        "timeline" => interview.timeline = Some(label.into()),
        "scope" => interview.scope = Some(label.into()),
        "constraints" => interview.constraints = Some(label.into()),
        "output" => interview.output = Some(label.into()),
        _ => {}
    }

    match interview.step {
        PlanningStep::Constraints if matches!(kind, "timeline" | "scope" | "constraints") => {
            let has_all = interview.timeline.is_some()
                && interview.scope.is_some()
                && interview.constraints.is_some();
            if has_all {
                interview.step = PlanningStep::Output;
                let _ = send_mrkdwn(
                    &session,
                    channel,
                    thread_ts,
                    "What format do you want the plan in: checklist, spec, or draft?",
                )
                .await;
            }
        }
        PlanningStep::Output if kind == "output" => {
            interview.step = PlanningStep::Summary;
            let summary = planning_summary_mrkdwn(interview);
            let _ = send_mrkdwn(&session, channel, thread_ts, &summary).await;
        }
        _ => {}
    }
}

// ── Pulse delivery ──────────────────────────────────────────────────

fn spawn_pulse_listener(state: SlackState) {
    let client = state.client.clone();
    let bot_token = state.bot_token.clone();
    let allowed_channels = state.config.allowed_channels.clone();
    let delivered_rx = state.handle.delivered_rx.clone();

    tokio::spawn(async move {
        let mut rx = delivered_rx.lock().await;
        while let Some(pulse) = rx.recv().await {
            let label = match pulse.source.label() {
                "heartbeat" => ":heartbeat: heartbeat",
                "memory_scan" => ":mag: insight",
                "idle_musing" => ":thought_balloon: thought",
                "conversation_reentry" => ":arrows_counterclockwise: follow-up",
                "mood_shift" => ":performing_arts: mood",
                "autonomous_task" => ":zap: task result",
                other => other,
            };
            let text = format!("*{}*\n{}", escape_mrkdwn(label), escape_mrkdwn(&pulse.content));

            let target_channels: Vec<String> = match &pulse.target {
                crate::pulse::PulseTarget::Session(ctx) if ctx.platform == "slack" => {
                    vec![ctx.chat_id.clone()]
                }
                _ => allowed_channels.clone(),
            };

            let session = client.open_session(&bot_token);
            for ch in &target_channels {
                let channel = SlackChannelId::from(ch.as_str());
                let content = SlackMessageContent::new().with_text(text.clone());
                let req = SlackApiChatPostMessageRequest::new(channel, content);
                let _ = session.chat_post_message(&req).await;
            }
        }
    });
}

// ── Entry point ─────────────────────────────────────────────────────

/// Entry point: run the Slack bot.
pub async fn run_slack(
    handle: CoreHandle,
    config: SlackConfig,
    system_info: SystemInfo,
) -> anyhow::Result<()> {
    let bot_token_str = config
        .bot_token
        .clone()
        .or_else(|| std::env::var("ATHENA_SLACK_BOT_TOKEN").ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Slack bot token not set. Set [slack].bot_token or ATHENA_SLACK_BOT_TOKEN env var"
            )
        })?;

    // Enforce allow_all safety
    if config.allowed_channels.is_empty() && !config.allow_all {
        return Err(anyhow::anyhow!(
            "Slack bot refused to start: allowed_channels is empty and allow_all is false. \
             Set allowed_channels to specific channel IDs or set allow_all = true in [slack] config."
        ));
    }

    if config.allow_all {
        tracing::warn!("allow_all is true — bot will respond to ALL channels");
    }

    let bot_token = SlackApiToken::new(SlackApiTokenValue::from(bot_token_str));
    let client = Arc::new(SlackClient::new(
        SlackClientHyperHttpsConnector::new()
            .map_err(|e| anyhow::anyhow!("Failed to create Slack HTTP connector: {}", e))?,
    ));

    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let last_request: Arc<Mutex<HashMap<String, tokio::time::Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let planning: Arc<Mutex<HashMap<String, PlanningInterview>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn stale confirmation cleanup task
    let cleanup_pending = pending.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let mut map = cleanup_pending.lock().await;
            if !map.is_empty() {
                map.retain(|_, tx| !tx.is_closed());
            }
        }
    });

    // Spawn stale planning cleanup task
    let cleanup_planning = planning.clone();
    let planning_timeout = tokio::time::Duration::from_secs(config.planning_timeout_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = tokio::time::Instant::now();
            {
                let mut map = cleanup_planning.lock().await;
                if !map.is_empty() {
                    map.retain(|_, interview| {
                        now.duration_since(interview.last_updated) < planning_timeout
                    });
                }
            }
        }
    });

    let state = SlackState {
        handle,
        client: client.clone(),
        bot_token: bot_token.clone(),
        pending,
        config: config.clone(),
        last_request,
        planning,
        system_info,
    };

    // Spawn pulse listener
    spawn_pulse_listener(state.clone());

    match config.mode.as_str() {
        "socket" => run_socket_mode(client, bot_token, config, state).await,
        "events_api" => run_events_api(config, state).await,
        other => Err(anyhow::anyhow!(
            "Unknown Slack mode: {}. Use 'socket' or 'events_api'",
            other
        )),
    }
}

async fn run_socket_mode(
    client: Arc<SlackHyperClient>,
    _bot_token: SlackApiToken,
    config: SlackConfig,
    state: SlackState,
) -> anyhow::Result<()> {
    let app_token_str = config
        .app_token
        .clone()
        .or_else(|| std::env::var("ATHENA_SLACK_APP_TOKEN").ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Socket Mode requires app_token. Set [slack].app_token or ATHENA_SLACK_APP_TOKEN env var"
            )
        })?;
    let app_token = SlackApiToken::new(SlackApiTokenValue::from(app_token_str));

    // Event handlers must be plain fn pointers (not closures) for slack-morphism.
    // We pass SlackState via the user state storage mechanism.

    fn push_events_handler(
        event: SlackPushEventCallback,
        _client: Arc<SlackHyperClient>,
        states: SlackClientEventsUserState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>> {
        Box::pin(async move {
            let state = {
                let guard = states.read().await;
                guard.get_user_state::<SlackState>().cloned()
            };
            if let Some(state) = state {
                handle_push_event(event, state).await?;
            }
            Ok(())
        })
    }

    fn command_events_handler(
        event: SlackCommandEvent,
        _client: Arc<SlackHyperClient>,
        states: SlackClientEventsUserState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<SlackCommandEventResponse, Box<dyn std::error::Error + Send + Sync>>> + Send>> {
        Box::pin(async move {
            let state = {
                let guard = states.read().await;
                guard.get_user_state::<SlackState>().cloned()
            };
            if let Some(state) = state {
                let text = event.text.clone().unwrap_or_default();
                let channel = event.channel_id.clone();
                let user = event.user_id.clone();
                tokio::spawn(async move {
                    handle_slash_command(&text, channel, user, state).await;
                });
            }
            Ok(SlackCommandEventResponse::new(
                SlackMessageContent::new()
                    .with_text("Processing...".to_string()),
            ))
        })
    }

    fn interaction_events_handler(
        event: SlackInteractionEvent,
        _client: Arc<SlackHyperClient>,
        states: SlackClientEventsUserState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>> {
        Box::pin(async move {
            let state = {
                let guard = states.read().await;
                guard.get_user_state::<SlackState>().cloned()
            };
            if let Some(state) = state {
                handle_interaction_event(event, state).await?;
            }
            Ok(())
        })
    }

    let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
        .with_push_events(push_events_handler)
        .with_command_events(command_events_handler)
        .with_interaction_events(interaction_events_handler);

    fn error_handler(
        err: Box<dyn std::error::Error + Send + Sync>,
        _client: Arc<SlackHyperClient>,
        _states: SlackClientEventsUserState,
    ) -> axum::http::StatusCode {
        tracing::error!(error = %err, "Slack socket mode error");
        axum::http::StatusCode::OK
    }

    let listener_environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(client)
            .with_error_handler(error_handler)
            .with_user_state(state),
    );

    let socket_mode_listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment,
        socket_mode_callbacks,
    );

    tracing::info!("Slack bot starting (Socket Mode)...");
    socket_mode_listener.listen_for(&app_token).await
        .map_err(|e| anyhow::anyhow!("Failed to start socket mode: {}", e))?;
    socket_mode_listener.serve().await;

    Ok(())
}

async fn run_events_api(
    _config: SlackConfig,
    _state: SlackState,
) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("Events API mode is not yet implemented. Use mode = \"socket\" instead."))
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_preview_truncates_long_output() {
        let long = "a".repeat(5000);
        let preview = stream_preview(&long);
        assert!(preview.len() <= 2803);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn tool_result_mrkdwn_strips_prefix_and_escapes() {
        let text = tool_result_mrkdwn("shell", "[tool result]\n<ok>", true);
        assert!(text.contains("*Behind the scenes — shell*"));
        assert!(text.contains("&lt;ok&gt;"));
        assert!(!text.contains("[tool result]"));
    }

    #[test]
    fn rate_limit_allows_and_blocks_by_cooldown() {
        let now = tokio::time::Instant::now();
        assert!(!should_rate_limit(
            None,
            now,
            tokio::time::Duration::from_secs(5)
        ));
        assert!(should_rate_limit(
            Some(now),
            now + tokio::time::Duration::from_secs(1),
            tokio::time::Duration::from_secs(5)
        ));
        assert!(!should_rate_limit(
            Some(now),
            now + tokio::time::Duration::from_secs(6),
            tokio::time::Duration::from_secs(5)
        ));
    }

    #[test]
    fn planning_detection_matches_keywords() {
        assert!(should_start_planning_interview("Need a plan for launch"));
        assert!(should_start_planning_interview("Planning a rollout"));
        assert!(!should_start_planning_interview("/plan"));
    }

    #[test]
    fn bare_plan_request_detection() {
        assert!(is_bare_plan_request("plan"));
        assert!(is_bare_plan_request("planning"));
        assert!(!is_bare_plan_request("plan onboarding flow"));
    }

    #[test]
    fn done_text_detection() {
        assert!(is_done_text("done"));
        assert!(is_done_text("Finished"));
        assert!(is_done_text("looks good"));
        assert!(is_done_text("that's it"));
        assert!(is_done_text("perfect"));
        assert!(!is_done_text("keep going"));
    }

    #[test]
    fn build_implement_prompt_standalone() {
        let prompt = build_implement_prompt("refactor auth", None);
        assert!(prompt.contains("Goal: refactor auth"));
        assert!(prompt.contains("Start with the first actionable item"));
        assert!(!prompt.contains("Constraints:"));
    }

    #[test]
    fn build_implement_prompt_with_plan() {
        let iv = PlanningInterview {
            step: PlanningStep::Refining,
            goal: Some("launch MVP".to_string()),
            constraints: Some("no budget".to_string()),
            timeline: Some("this week".to_string()),
            scope: Some("implementation".to_string()),
            output: None,
            depth: None,
            last_updated: tokio::time::Instant::now(),
        };
        let prompt = build_implement_prompt("launch MVP", Some(&iv));
        assert!(prompt.contains("Goal: launch MVP"));
        assert!(prompt.contains("Constraints: no budget"));
        assert!(prompt.contains("Timeline: this week"));
        assert!(prompt.contains("Scope: implementation"));
    }

    #[test]
    fn escape_mrkdwn_special_chars() {
        assert_eq!(escape_mrkdwn("a & b"), "a &amp; b");
        assert_eq!(escape_mrkdwn("<script>"), "&lt;script&gt;");
        // Formatting chars should be neutralized
        let escaped = escape_mrkdwn("*bold* _italic_ ~strike~ `code`");
        assert!(!escaped.contains('*'));
        assert!(!escaped.contains('_'));
        assert!(!escaped.contains('~'));
        assert!(!escaped.contains('`'));
    }

    #[test]
    fn chunk_message_splits_at_boundary() {
        let text = "hello\nworld\nfoo\n";
        let chunks = chunk_message(text, 12);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= 12);
        }
    }

    #[test]
    fn is_authorized_with_allowed_channels() {
        let config = SlackConfig {
            allowed_channels: vec!["C123".to_string()],
            allow_all: false,
            ..Default::default()
        };
        assert!(is_authorized("C123", &config));
        assert!(!is_authorized("C456", &config));
    }

    #[test]
    fn is_authorized_with_allow_all() {
        let config = SlackConfig {
            allowed_channels: vec![],
            allow_all: true,
            ..Default::default()
        };
        assert!(is_authorized("C123", &config));
        assert!(is_authorized("C456", &config));
    }

    #[test]
    fn is_authorized_denies_by_default() {
        let config = SlackConfig {
            allowed_channels: vec![],
            allow_all: false,
            ..Default::default()
        };
        assert!(!is_authorized("C123", &config));
    }

    #[test]
    fn format_duration_renders_correctly() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(90061), "1d 1h 1m");
    }

    #[test]
    fn format_tokens_renders_correctly() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1000), "1k");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(1000000), "1M");
        assert_eq!(format_tokens(1500000), "1.5M");
    }

    #[test]
    fn energy_bar_renders_correctly() {
        let bar = energy_bar(0.5);
        assert!(bar.contains("█████"));
        assert!(bar.contains("░░░░░"));
    }
}
