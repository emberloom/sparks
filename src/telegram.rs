use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

use teloxide::prelude::*;
use teloxide::types::{BotCommand, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

use crate::config::TelegramConfig;
use crate::confirm::Confirmer;
use crate::core::{CoreEvent, CoreHandle, SessionContext};
use crate::error::{AthenaError, Result};
use crate::observer::ObserverCategory;

/// System info passed from main to the Telegram bot.
#[derive(Clone)]
pub struct SystemInfo {
    pub provider: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub started_at: tokio::time::Instant,
}

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

// ── HTML formatting helpers ──────────────────────────────────────────

/// Escape text for safe inclusion in Telegram HTML messages.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

/// Send an HTML-formatted message, chunking if needed.
async fn send_html(bot: &Bot, chat_id: ChatId, html: &str) -> ResponseResult<()> {
    for chunk in chunk_message(html, 4000) {
        bot.send_message(chat_id, chunk)
            .parse_mode(ParseMode::Html)
            .await?;
    }
    Ok(())
}

/// Transcribe a Telegram voice message using a Whisper-compatible STT API.
async fn transcribe_voice(
    bot: &Bot,
    voice: &teloxide::types::Voice,
    config: &TelegramConfig,
) -> std::result::Result<String, String> {
    let stt_url = config
        .stt_url
        .as_deref()
        .ok_or("Voice messages require STT configuration. Set [telegram] stt_url in config.")?;
    let stt_key = config
        .stt_api_key
        .clone()
        .or_else(|| std::env::var("ATHENA_STT_API_KEY").ok())
        .ok_or("STT API key not configured. Set stt_api_key or ATHENA_STT_API_KEY env var.")?;
    let stt_model = config.stt_model.as_deref().unwrap_or("whisper-large-v3");

    // Download voice file from Telegram
    let file = bot
        .get_file(&voice.file.id)
        .await
        .map_err(|e| format!("Failed to get voice file: {}", e))?;

    let download_url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        bot.token(),
        file.path
    );
    let bytes = reqwest::get(&download_url)
        .await
        .map_err(|e| format!("Failed to download voice: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("Failed to read voice data: {}", e))?;

    // POST to Whisper-compatible STT API
    let part = reqwest::multipart::Part::bytes(bytes.to_vec())
        .file_name("voice.ogg")
        .mime_str("audio/ogg")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", stt_model.to_string());

    let client = reqwest::Client::new();
    let resp = client
        .post(stt_url)
        .bearer_auth(&stt_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("STT request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("STT error ({}): {}", status, body));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse STT response: {}", e))?;

    json["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "STT response missing 'text' field".to_string())
}

// ── Confirmer ────────────────────────────────────────────────────────

/// Telegram confirmer: sends inline keyboard, waits on oneshot with timeout.
pub struct TelegramConfirmer {
    bot: Bot,
    chat_id: ChatId,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout_secs: u64,
}

#[async_trait::async_trait]
impl Confirmer for TelegramConfirmer {
    async fn confirm(&self, action: &str) -> Result<bool> {
        let confirm_id = uuid::Uuid::new_v4().to_string();

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("Approve", format!("confirm:{}:yes", confirm_id)),
            InlineKeyboardButton::callback("Deny", format!("confirm:{}:no", confirm_id)),
        ]]);

        let text = format!(
            "<b>Action requires approval:</b>\n<code>{}</code>",
            escape_html(action)
        );

        self.bot
            .send_message(self.chat_id, &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await
            .map_err(|e| AthenaError::Tool(format!("Failed to send confirmation: {}", e)))?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(confirm_id.clone(), tx);
        }

        let timeout = tokio::time::Duration::from_secs(self.timeout_secs);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(true)) => Ok(true),
            Ok(Ok(false)) | Err(_) => {
                // Timed out or denied — clean up
                let mut pending = self.pending.lock().await;
                pending.remove(&confirm_id);
                Err(AthenaError::Cancelled)
            }
            Ok(Err(_)) => {
                // Channel dropped
                Err(AthenaError::Cancelled)
            }
        }
    }
}

// ── State ────────────────────────────────────────────────────────────

/// Shared state for the Telegram bot.
#[derive(Clone)]
struct TelegramState {
    handle: CoreHandle,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    config: TelegramConfig,
    /// M7: Per-chat rate limiting — tracks last request time per chat
    last_request: Arc<Mutex<HashMap<i64, tokio::time::Instant>>>,
    /// Planning interviews keyed by chat id
    planning: Arc<Mutex<HashMap<i64, PlanningInterview>>>,
    system_info: SystemInfo,
}

/// L3: Split a message into chunks at `max` bytes, respecting UTF-8 char boundaries.
fn chunk_message(text: &str, max: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + max).min(text.len());
        // Walk back to a valid char boundary if we landed in the middle of a character
        let mut actual_end = end;
        while actual_end > start && !text.is_char_boundary(actual_end) {
            actual_end -= 1;
        }
        // Try to break at a newline if possible
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

/// Check if a chat is authorized
fn is_authorized(chat_id: i64, config: &TelegramConfig) -> bool {
    if !config.allowed_chats.is_empty() {
        return config.allowed_chats.contains(&chat_id);
    }
    // If allowed_chats is empty, only allow if allow_all is explicitly set
    config.allow_all
}

// ── Planning interview helpers ───────────────────────────────────────

const PLAN_KEYWORDS: &[&str] = &[
    "plan",
    "planning",
    "roadmap",
    "strategy",
    "launch",
    "rollout",
    "gtm",
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
    matches!(lowered.as_str(), "plan" | "planning" | "make a plan" | "need a plan")
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

fn planning_constraints_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Today", "plan:timeline:today"),
            InlineKeyboardButton::callback("This week", "plan:timeline:week"),
            InlineKeyboardButton::callback("No deadline", "plan:timeline:none"),
        ],
        vec![
            InlineKeyboardButton::callback("Idea only", "plan:scope:idea"),
            InlineKeyboardButton::callback("Implementation", "plan:scope:impl"),
            InlineKeyboardButton::callback("Full plan", "plan:scope:full"),
        ],
        vec![
            InlineKeyboardButton::callback("No constraints", "plan:constraints:none"),
            InlineKeyboardButton::callback("Skip", "plan:skip:constraints"),
        ],
    ])
}

fn planning_output_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Checklist", "plan:output:checklist"),
        InlineKeyboardButton::callback("Spec", "plan:output:spec"),
        InlineKeyboardButton::callback("Draft", "plan:output:draft"),
    ]])
}

fn planning_confirm_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Confirm", "plan:confirm:yes"),
        InlineKeyboardButton::callback("Edit", "plan:confirm:edit"),
        InlineKeyboardButton::callback("Cancel", "plan:confirm:cancel"),
    ]])
}

fn planning_post_generate_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Implement", "plan:post:implement"),
        InlineKeyboardButton::callback("Refine", "plan:post:refine"),
        InlineKeyboardButton::callback("Done", "plan:post:done"),
    ]])
}

fn planning_summary_html(interview: &PlanningInterview) -> String {
    let goal = interview
        .goal
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("unspecified");
    let constraints = interview
        .constraints
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("none");
    let output = interview
        .output
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("checklist");
    let timeline = interview
        .timeline
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("unspecified");
    let depth = interview
        .depth
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("standard");
    let scope = interview
        .scope
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("unspecified");

    format!(
        "<b>Here’s what I have so far:</b>\n\n         • <b>Goal:</b> {}\n         • <b>Constraints:</b> {}\n         • <b>Timeline:</b> {}\n         • <b>Scope:</b> {}\n         • <b>Depth:</b> {}\n         • <b>Output:</b> {}\n\nConfirm or edit anything above.",
        escape_html(goal),
        escape_html(constraints),
        escape_html(timeline),
        escape_html(scope),
        escape_html(depth),
        escape_html(output),
    )
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
    out.push_str("\nFinish with: 'Ready to implement this plan, or would you like to refine anything?'");
    out
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

// ── CLI tool picker helpers ──────────────────────────────────────────

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

fn build_cli_keyboard(current: &str) -> InlineKeyboardMarkup {
    let buttons: Vec<InlineKeyboardButton> = CLI_TOOLS
        .iter()
        .map(|(id, name)| {
            let label = if *id == current {
                format!("{} \u{2713}", name)
            } else {
                name.to_string()
            };
            InlineKeyboardButton::callback(label, format!("cli:{}", id))
        })
        .collect();
    InlineKeyboardMarkup::new(vec![buttons])
}

fn session_user_id(msg: &Message) -> String {
    msg.from
        .as_ref()
        .map(|u| u.id.0.to_string())
        .unwrap_or_else(|| "unknown".into())
}

async fn command_help(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let help = "<b>Athena</b>

        <b>Commands:</b>
        /plan — Start a planning interview
        /status — System status &amp; uptime
        /model — Show/switch LLM model
        /models — List available models from API
        /cli_model — Show/switch model for CLI tools
        /knobs — Runtime knob values
        /mood — Current mood state
        /jobs — Scheduled cron jobs
        /session — Current session info
        /set <code>&lt;key&gt; &lt;value&gt;</code> — Modify a runtime knob
        /cli — Switch coding CLI tool
        /ghosts — List active ghosts
        /memories — List saved memories
        /dispatch <code>&lt;ghost&gt; &lt;goal&gt;</code> — Run an autonomous task
        /help — This help message

        Send any message to chat with Athena.";
    send_html(bot, chat_id, help).await
}

async fn command_plan(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<()> {
    if !state.config.planning_enabled {
        send_html(
            bot,
            chat_id,
            "<i>Planning interview is disabled in config.</i>",
        )
        .await?;
        return Ok(());
    }

    let arg = text.strip_prefix("/plan").unwrap_or_default().trim();
    if matches!(arg, "cancel" | "stop") {
        let mut planning = state.planning.lock().await;
        planning.remove(&chat_id.0);
        send_html(bot, chat_id, "<i>Planning cancelled.</i>").await?;
        return Ok(());
    }

    let now = tokio::time::Instant::now();
    let mut interview = PlanningInterview::new(now);
    let (prompt, keyboard) = if arg.is_empty() {
        (
            "<b>Quick planning interview</b>\n\nWhat does success look like? One sentence is fine."
                .to_string(),
            None,
        )
    } else {
        interview.goal = Some(arg.to_string());
        interview.step = PlanningStep::Constraints;
        (
            "Got it. A couple quick questions to sharpen the plan.\n\nAny constraints I should respect? (timeline, budget, stack, scope)"
                .to_string(),
            Some(planning_constraints_keyboard()),
        )
    };

    {
        let mut planning = state.planning.lock().await;
        planning.insert(chat_id.0, interview);
    }

    send_planning_prompt(bot, chat_id, prompt, keyboard).await
}

async fn command_ghosts(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    let ghosts = state.handle.list_ghosts();
    let mut out = String::from(
        "<b>Active ghosts:</b>

",
    );
    for g in &ghosts {
        out.push_str(&format!(
            "• <b>{}</b> — {} <code>[{}]</code>
",
            escape_html(&g.name),
            escape_html(&g.description),
            escape_html(&g.tools.join(", "))
        ));
    }
    send_html(bot, chat_id, &out).await
}

async fn command_memories(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    match state.handle.list_memories() {
        Ok(memories) if memories.is_empty() => {
            send_html(bot, chat_id, "<i>No memories.</i>").await?;
        }
        Ok(memories) => {
            let mut out = String::from(
                "<b>Memories:</b>

",
            );
            for m in &memories {
                out.push_str(&format!(
                    "<code>[{}]</code> <b>{}</b> — {}
",
                    escape_html(&m.id[..8]),
                    escape_html(&m.category),
                    escape_html(&m.content)
                ));
            }
            send_html(bot, chat_id, &out).await?;
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list memories");
            send_html(bot, chat_id, "<i>An internal error occurred.</i>").await?;
        }
    }
    Ok(())
}

async fn command_status(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
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

    let credits_line = match state.handle.llm.credits().await {
        Ok(Some((total, used))) => {
            let remaining = total - used;
            format!(
                "\n<b>Credits:</b> <code>${:.2} remaining</code> (${:.2} used of ${:.2})",
                remaining, used, total
            )
        }
        _ => String::new(),
    };

    let html = format!(
        "<b>System Status</b>\n\n         <b>Provider:</b> <code>{}</code>\n         <b>Model:</b> <code>{}</code>\n         <b>Context:</b> <code>{} tokens</code>\n         <b>Temperature:</b> <code>{}</code>\n         <b>Max tokens:</b> <code>{}</code>\n         <b>Uptime:</b> <code>{}</code>\n         <b>Last active:</b> <code>{}</code>\n         <b>Ghosts:</b> <code>{}</code>\n         <b>Mood:</b> {}{credits}",
        escape_html(&info.provider),
        escape_html(&current_model),
        ctx_label,
        info.temperature,
        info.max_tokens,
        format_duration(uptime_secs),
        escape_html(&last_active),
        ghost_count,
        escape_html(&mood_desc),
        credits = credits_line,
    );
    send_html(bot, chat_id, &html).await
}

async fn command_knobs(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    let display = {
        let k = state.handle.knobs.read().unwrap();
        k.display()
    };
    let html = format!("<pre>{}</pre>", escape_html(&display));
    send_html(bot, chat_id, &html).await
}

async fn command_mood(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    let desc = state.handle.mood.describe();
    let energy = state.handle.mood.energy();
    let modifier = state.handle.mood.modifier();

    let html = format!(
        "<b>Mood</b>\n\n         {}\n         <b>Energy:</b> <code>{} {:.0}%</code>\n         <b>Modifier:</b> <code>{}</code>",
        escape_html(&desc),
        energy_bar(energy),
        energy * 100.0,
        escape_html(&modifier),
    );
    send_html(bot, chat_id, &html).await
}

async fn command_jobs(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    if let Some(engine) = &state.handle.cron_engine {
        match engine.list_jobs() {
            Ok(jobs) if jobs.is_empty() => {
                send_html(bot, chat_id, "<i>No scheduled jobs.</i>").await?;
            }
            Ok(jobs) => {
                let mut out = String::from("<b>Scheduled Jobs:</b>\n\n");
                for j in &jobs {
                    let status = if j.enabled { "on" } else { "off" };
                    let next = j
                        .next_run
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".to_string());
                    out.push_str(&format!(
                        "<code>[{}]</code> <b>{}</b> ({})\n  next: {} — {}\n",
                        escape_html(&j.id[..8]),
                        escape_html(&j.name),
                        status,
                        escape_html(&next),
                        escape_html(&j.prompt),
                    ));
                }
                send_html(bot, chat_id, &out).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to list jobs");
                send_html(bot, chat_id, "<i>Failed to list jobs.</i>").await?;
            }
        }
    } else {
        send_html(bot, chat_id, "<i>Cron engine not initialized.</i>").await?;
    }
    Ok(())
}

async fn command_session(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    state: &TelegramState,
) -> ResponseResult<()> {
    let session_key = format!("telegram:{}:{}", session_user_id(msg), chat_id.0);
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

    let html = format!(
        "<b>Session</b>\n\n         <b>Key:</b> <code>{}</code>\n         <b>Model:</b> <code>{}</code>\n         <b>Turns:</b> <code>{}</code>\n         <b>Est. tokens:</b> <code>~{}</code>\n         <b>Context:</b> <code>{:.0}%</code> of <code>{}</code>\n         <b>Last message:</b>\n<i>{}</i>",
        escape_html(&session_key),
        escape_html(&current_model),
        turn_count,
        format_tokens(est_tokens as u64),
        utilization,
        format_tokens(context_window),
        escape_html(&last_preview),
    );
    send_html(bot, chat_id, &html).await
}

async fn command_cli(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    let current = state.handle.knobs.read().unwrap().cli_tool.clone();
    let keyboard = build_cli_keyboard(&current);
    let label = cli_display_name(&current);
    let html = format!("<b>Coding CLI tool:</b> {}", escape_html(&label));
    bot.send_message(chat_id, html)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn command_set(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<()> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.len() {
        1 => command_knobs(bot, chat_id, state).await?,
        3 => {
            let result = {
                let mut k = state.handle.knobs.write().unwrap();
                k.set(parts[1], parts[2])
            };
            match result {
                Ok(msg) => {
                    state
                        .handle
                        .observer
                        .emit(crate::observer::ObserverEvent::new(
                            ObserverCategory::KnobChange,
                            format!("{} = {}", parts[1], parts[2]),
                        ));
                    let html = format!("<b>Set:</b> {}", escape_html(&msg));
                    send_html(bot, chat_id, &html).await?;
                }
                Err(e) => {
                    let html = format!("<i>Error: {}</i>", escape_html(&e));
                    send_html(bot, chat_id, &html).await?;
                }
            }
        }
        _ => {
            send_html(
                bot,
                chat_id,
                "<i>Usage: /set or /set &lt;key&gt; &lt;value&gt;</i>",
            )
            .await?;
        }
    }
    Ok(())
}

async fn command_dispatch(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<()> {
    let rest = text.strip_prefix("/dispatch").unwrap_or_default().trim();
    if rest.is_empty() {
        send_html(
            bot,
            chat_id,
            "<i>Usage: /dispatch &lt;ghost&gt; &lt;goal&gt;</i>",
        )
        .await?;
        return Ok(());
    }

    let (ghost_name, goal) = match rest.split_once(' ') {
        Some((g, goal)) => (Some(g.to_string()), goal.to_string()),
        None => (None, rest.to_string()),
    };

    let target = crate::pulse::PulseTarget::Session(crate::core::SessionContext {
        platform: "telegram".into(),
        user_id: session_user_id(msg),
        chat_id: chat_id.0.to_string(),
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
            let html = format!("<i>Failed to dispatch: {}</i>", escape_html(&e.to_string()));
            send_html(bot, chat_id, &html).await?;
        }
        Ok(task_id) => {
            let label = ghost_name.unwrap_or_else(|| "auto".into());
            let html = format!(
                "<i>⚡ Dispatched to {} (task_id={})</i>",
                escape_html(&label),
                escape_html(&task_id)
            );
            send_html(bot, chat_id, &html).await?;
        }
    }
    Ok(())
}

async fn command_model(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<()> {
    if text == "/model" {
        let current = state.handle.llm.current_model();
        let html = format!(
            "<b>Current model:</b> <code>{}</code>",
            escape_html(&current)
        );
        send_html(bot, chat_id, &html).await?;
        return Ok(());
    }

    let arg = text.strip_prefix("/model ").unwrap_or_default().trim();
    if arg == "reset" {
        state.handle.llm.set_model_override(None);
        let current = state.handle.llm.current_model();
        let html = format!(
            "<b>Reset to default:</b> <code>{}</code>",
            escape_html(&current)
        );
        send_html(bot, chat_id, &html).await?;
    } else {
        state.handle.llm.set_model_override(Some(arg.to_string()));
        let html = format!("<b>Model set to:</b> <code>{}</code>", escape_html(arg));
        send_html(bot, chat_id, &html).await?;
    }
    Ok(())
}

async fn command_models(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> ResponseResult<()> {
    match state.handle.llm.list_models().await {
        Ok(models) if models.is_empty() => {
            send_html(bot, chat_id, "<i>No models returned by API.</i>").await?;
        }
        Ok(models) => {
            let current = state.handle.llm.current_model();
            let mut out = String::from("<b>Available models:</b>\n\n");
            for m in &models {
                if *m == current {
                    out.push_str(&format!("• <b>{}</b> (active)\n", escape_html(m)));
                } else {
                    out.push_str(&format!("• <code>{}</code>\n", escape_html(m)));
                }
            }
            send_html(bot, chat_id, &out).await?;
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list models");
            send_html(bot, chat_id, "<i>Failed to list models.</i>").await?;
        }
    }
    Ok(())
}

async fn command_cli_model(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<()> {
    if text == "/cli_model" {
        let model = state.handle.knobs.read().unwrap().cli_model.clone();
        let label = if model.is_empty() {
            "default (tool decides)".to_string()
        } else {
            model
        };
        let html = format!(
            "<b>CLI tool model:</b> <code>{}</code>",
            escape_html(&label)
        );
        send_html(bot, chat_id, &html).await?;
        return Ok(());
    }

    let arg = text.strip_prefix("/cli_model ").unwrap_or_default().trim();
    let result = {
        let mut k = state.handle.knobs.write().unwrap();
        k.set("cli_model", arg)
    };
    match result {
        Ok(msg) => {
            let html = format!("<b>Set:</b> {}", escape_html(&msg));
            send_html(bot, chat_id, &html).await?;
        }
        Err(e) => {
            let html = format!("<i>Error: {}</i>", escape_html(&e));
            send_html(bot, chat_id, &html).await?;
        }
    }
    Ok(())
}

async fn handle_slash_commands(
    bot: &Bot,
    msg: &Message,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<bool> {
    if text == "/start" || text == "/help" {
        command_help(bot, chat_id).await?;
        return Ok(true);
    }
    if text == "/plan" || text.starts_with("/plan ") {
        command_plan(bot, chat_id, text, state).await?;
        return Ok(true);
    }
    if text == "/ghosts" {
        command_ghosts(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/memories" {
        command_memories(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/status" {
        command_status(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/knobs" {
        command_knobs(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/mood" {
        command_mood(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/jobs" {
        command_jobs(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/session" {
        command_session(bot, chat_id, msg, state).await?;
        return Ok(true);
    }
    if text == "/cli" {
        command_cli(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text.starts_with("/set") {
        command_set(bot, chat_id, text, state).await?;
        return Ok(true);
    }
    if text.starts_with("/dispatch") {
        command_dispatch(bot, chat_id, msg, text, state).await?;
        return Ok(true);
    }
    if text == "/model" || text.starts_with("/model ") {
        command_model(bot, chat_id, text, state).await?;
        return Ok(true);
    }
    if text == "/models" {
        command_models(bot, chat_id, state).await?;
        return Ok(true);
    }
    if text == "/cli_model" || text.starts_with("/cli_model ") {
        command_cli_model(bot, chat_id, text, state).await?;
        return Ok(true);
    }
    Ok(false)
}

async fn extract_message_text(
    bot: &Bot,
    msg: &Message,
    chat_id: ChatId,
    config: &TelegramConfig,
) -> ResponseResult<Option<String>> {
    if let Some(t) = msg.text() {
        return Ok(Some(t.to_string()));
    }
    if let Some(voice) = msg.voice() {
        return match transcribe_voice(bot, voice, config).await {
            Ok(transcript) => {
                send_html(
                    bot,
                    chat_id,
                    &format!("<i>🎙 {}</i>", escape_html(&transcript)),
                )
                .await?;
                Ok(Some(transcript))
            }
            Err(e) => {
                send_html(bot, chat_id, &format!("<i>{}</i>", escape_html(&e))).await?;
                Ok(None)
            }
        };
    }
    if msg.photo().is_some() {
        send_html(
            bot,
            chat_id,
            "<i>I can't process images yet — please describe what you need in text.</i>",
        )
        .await?;
        return Ok(None);
    }
    if msg.document().is_some() {
        send_html(
            bot,
            chat_id,
            "<i>I can't process files yet — please paste the relevant content as text.</i>",
        )
        .await?;
        return Ok(None);
    }
    Ok(None)
}

async fn check_rate_limit(
    state: &TelegramState,
    bot: &Bot,
    chat_id: ChatId,
) -> ResponseResult<bool> {
    let mut last_req = state.last_request.lock().await;
    let now = tokio::time::Instant::now();
    if should_rate_limit(
        last_req.get(&chat_id.0).copied(),
        now,
        tokio::time::Duration::from_secs(5),
    ) {
        send_html(
            bot,
            chat_id,
            "<i>Please wait a few seconds before sending another request.</i>",
        )
        .await?;
        return Ok(false);
    }
    last_req.insert(chat_id.0, now);
    Ok(true)
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

fn telegram_confirmer(bot: &Bot, chat_id: ChatId, state: &TelegramState) -> Arc<dyn Confirmer> {
    Arc::new(TelegramConfirmer {
        bot: bot.clone(),
        chat_id,
        pending: state.pending.clone(),
        timeout_secs: state.config.confirm_timeout_secs,
    })
}

fn telegram_session_from_user(user_id: String, chat_id: ChatId) -> SessionContext {
    SessionContext {
        platform: "telegram".into(),
        user_id,
        chat_id: chat_id.0.to_string(),
    }
}

fn telegram_session(msg: &Message, chat_id: ChatId) -> SessionContext {
    let user_id = msg
        .from
        .as_ref()
        .map(|u| u.id.0.to_string())
        .unwrap_or_else(|| "unknown".into());
    telegram_session_from_user(user_id, chat_id)
}

fn stream_preview(buffer: &str) -> String {
    let preview = escape_html(buffer);
    if preview.len() > 3800 {
        format!("{}...", &preview[..preview.floor_char_boundary(3800)])
    } else {
        preview
    }
}

fn tool_result_html(tool: &str, result: &str, success: bool) -> String {
    let icon = if success { "\u{2705}" } else { "\u{274c}" };
    let body = result
        .strip_prefix("[tool result]\n")
        .or_else(|| result.strip_prefix("[tool error]\n"))
        .unwrap_or(result);
    let display = if body.len() > 1500 {
        format!(
            "{}...\n<i>[{} chars total]</i>",
            escape_html(&body[..body.floor_char_boundary(1500)]),
            body.len()
        )
    } else {
        escape_html(body)
    };
    format!(
        "{} <b>Behind the scenes — {}</b>\n<blockquote><pre>{}</pre></blockquote>",
        icon,
        escape_html(tool),
        display,
    )
}

async fn send_response_chunks(bot: &Bot, chat_id: ChatId, response_text: &str) {
    let escaped = escape_html(response_text);
    for chunk in chunk_message(&escaped, 4000) {
        let _ = bot
            .send_message(chat_id, chunk)
            .parse_mode(ParseMode::Html)
            .await;
    }
}

async fn send_planning_prompt(
    bot: &Bot,
    chat_id: ChatId,
    html: String,
    keyboard: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    let mut req = bot.send_message(chat_id, html).parse_mode(ParseMode::Html);
    if let Some(kb) = keyboard {
        req = req.reply_markup(kb);
    }
    req.await?;
    Ok(())
}

async fn dispatch_to_core(
    bot: Bot,
    chat_id: ChatId,
    state: TelegramState,
    session: SessionContext,
    text: String,
    initial_status: &str,
) -> ResponseResult<()> {
    dispatch_to_core_with_followup(bot, chat_id, state, session, text, initial_status, None).await
}

async fn dispatch_to_core_with_followup(
    bot: Bot,
    chat_id: ChatId,
    state: TelegramState,
    session: SessionContext,
    text: String,
    initial_status: &str,
    followup: Option<(String, InlineKeyboardMarkup)>,
) -> ResponseResult<()> {
    let confirmer = telegram_confirmer(&bot, chat_id, &state);

    tracing::debug!("Sending status message");
    let status_msg = bot
        .send_message(chat_id, initial_status)
        .parse_mode(ParseMode::Html)
        .await?;
    tracing::debug!(msg_id = %status_msg.id, "Status message sent");

    tracing::debug!("Sending to core via handle.chat()");
    let events = match state.handle.chat(session, &text, confirmer).await {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!(error = %e, "handle.chat() failed");
            let _ = bot
                .edit_message_text(
                    chat_id,
                    status_msg.id,
                    "<i>An error occurred while processing your request.</i>",
                )
                .parse_mode(ParseMode::Html)
                .await;
            return Ok(());
        }
    };

    tokio::spawn(async move {
        forward_telegram_events(bot.clone(), chat_id, status_msg.id, events).await;
        if let Some((msg, kb)) = followup {
            let _ = bot
                .send_message(chat_id, msg)
                .parse_mode(ParseMode::Html)
                .reply_markup(kb)
                .await;
        }
    });

    Ok(())
}

async fn handle_planning_message(
    bot: &Bot,
    msg: &Message,
    chat_id: ChatId,
    text: &str,
    state: &TelegramState,
) -> ResponseResult<bool> {
    if !state.config.planning_enabled {
        return Ok(false);
    }

    enum Action {
        None,
        Prompt(String, Option<InlineKeyboardMarkup>),
        Dispatch(String),
        Cancelled,
        Done,
    }

    let now = tokio::time::Instant::now();
    let mut action = Action::None;

    {
        let mut planning = state.planning.lock().await;
        if let Some(interview) = planning.get_mut(&chat_id.0) {
            interview.last_updated = now;

            if is_cancel_text(text) {
                planning.remove(&chat_id.0);
                action = Action::Cancelled;
            } else {
                match interview.step {
                    PlanningStep::Goal => {
                        if is_skip_text(text) {
                            action = Action::Prompt(
                                "I need a goal to plan against. What does success look like?"
                                    .to_string(),
                                None,
                            );
                        } else {
                            interview.goal = Some(text.to_string());
                            interview.step = PlanningStep::Constraints;
                            action = Action::Prompt(
                                "Any constraints I should respect? (timeline, budget, stack, scope)"
                                    .to_string(),
                                Some(planning_constraints_keyboard()),
                            );
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
                        action = Action::Prompt(
                            "What format do you want the plan in: checklist, spec, or draft?"
                                .to_string(),
                            Some(planning_output_keyboard()),
                        );
                    }
                    PlanningStep::Output => {
                        if is_skip_text(text) {
                            interview.output = Some("checklist".to_string());
                        } else {
                            interview.output = Some(text.to_string());
                        }
                        interview.step = PlanningStep::Summary;
                        action =
                            Action::Prompt(planning_summary_html(interview), Some(planning_confirm_keyboard()));
                    }
                    PlanningStep::Summary => {
                        if is_confirm_text(text) {
                            let prompt = planning_build_prompt(interview);
                            interview.step = PlanningStep::Refining;
                            action = Action::Dispatch(prompt);
                        } else if is_edit_text(text) {
                            interview.step = PlanningStep::Editing;
                            action = Action::Prompt(
                                "Send corrections using lines like:\n<code>Goal: ...</code>\n<code>Constraints: ...</code>\n<code>Timeline: ...</code>\n<code>Scope: ...</code>\n<code>Depth: ...</code>\n<code>Output: ...</code>"
                                    .to_string(),
                                None,
                            );
                        } else if apply_planning_edits(interview, text) {
                            action =
                                Action::Prompt(planning_summary_html(interview), Some(planning_confirm_keyboard()));
                        } else {
                            action = Action::Prompt(
                                "Reply <code>confirm</code> to proceed, or send edits like <code>Goal: ...</code>."
                                    .to_string(),
                                Some(planning_confirm_keyboard()),
                            );
                        }
                    }
                    PlanningStep::Editing => {
                        if apply_planning_edits(interview, text) {
                            interview.step = PlanningStep::Summary;
                            action =
                                Action::Prompt(planning_summary_html(interview), Some(planning_confirm_keyboard()));
                        } else {
                            action = Action::Prompt(
                                "I couldn't read those edits. Try lines like <code>Goal: ...</code> or <code>Constraints: ...</code>."
                                    .to_string(),
                                None,
                            );
                        }
                    }
                    PlanningStep::Refining => {
                        if is_done_text(text) {
                            planning.remove(&chat_id.0);
                            action = Action::Done;
                        } else {
                            action = Action::Dispatch(text.to_string());
                        }
                    }
                }
            }
        } else if state.config.planning_auto && should_start_planning_interview(text) {
            let mut interview = PlanningInterview::new(now);
            if !is_bare_plan_request(text) {
                interview.goal = Some(text.to_string());
                interview.step = PlanningStep::Constraints;
                planning.insert(chat_id.0, interview.clone());
                action = Action::Prompt(
                    "Got it. A couple quick questions to sharpen the plan.\n\nAny constraints I should respect? (timeline, budget, stack, scope)"
                        .to_string(),
                    Some(planning_constraints_keyboard()),
                );
            } else {
                planning.insert(chat_id.0, interview.clone());
                action = Action::Prompt(
                    "<b>Quick planning interview</b>\n\nWhat does success look like? One sentence is fine."
                        .to_string(),
                    None,
                );
            }
        }
    }

    match action {
        Action::None => Ok(false),
        Action::Cancelled => {
            send_html(bot, chat_id, "<i>Planning cancelled.</i>").await?;
            Ok(true)
        }
        Action::Prompt(html, keyboard) => {
            send_planning_prompt(bot, chat_id, html, keyboard).await?;
            Ok(true)
        }
        Action::Dispatch(prompt) => {
            let session = telegram_session(msg, chat_id);
            dispatch_to_core_with_followup(
                bot.clone(),
                chat_id,
                state.clone(),
                session,
                prompt,
                "<i>Status: Drafting plan…</i>",
                Some((
                    "<b>Plan generated.</b> What would you like to do next?".to_string(),
                    planning_post_generate_keyboard(),
                )),
            )
            .await?;
            Ok(true)
        }
        Action::Done => {
            send_html(bot, chat_id, "<i>Plan finalised.</i>").await?;
            Ok(true)
        }
    }
}

async fn forward_telegram_events(
    bot: Bot,
    chat_id: ChatId,
    status_id: teloxide::types::MessageId,
    mut events: tokio::sync::mpsc::Receiver<CoreEvent>,
) {
    let mut stream_buffer = String::new();
    let mut last_edit = tokio::time::Instant::now();
    let edit_interval = tokio::time::Duration::from_millis(500);

    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Status(s) => {
                let _ = bot
                    .edit_message_text(
                        chat_id,
                        status_id,
                        format!("<i>Status: {}</i>", escape_html(&s)),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            CoreEvent::StreamChunk(chunk) => {
                stream_buffer.push_str(&chunk);
                let now = tokio::time::Instant::now();
                if now.duration_since(last_edit) >= edit_interval {
                    let display = stream_preview(&stream_buffer);
                    let _ = bot
                        .edit_message_text(chat_id, status_id, format!("<pre>{}</pre>", display))
                        .parse_mode(ParseMode::Html)
                        .await;
                    last_edit = now;
                }
            }
            CoreEvent::ToolRun {
                tool,
                result,
                success,
            } => {
                let html = tool_result_html(&tool, &result, success);
                let _ = send_html(&bot, chat_id, &html).await;
            }
            CoreEvent::Response(r) => {
                let _ = bot.delete_message(chat_id, status_id).await;
                let response_text = if !stream_buffer.is_empty() {
                    stream_buffer.clone()
                } else {
                    r
                };
                if response_text.trim().is_empty() {
                    break;
                }
                send_response_chunks(&bot, chat_id, &response_text).await;
            }
            CoreEvent::Error(e) => {
                tracing::error!(error = %e, chat_id = chat_id.0, "Task error");
                let _ = bot
                    .edit_message_text(
                        chat_id,
                        status_id,
                        "<i>An error occurred while processing your request.</i>",
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            CoreEvent::Pulse(p) => {
                let html = format!("<b>💬 pulse</b>\n{}", escape_html(&p));
                let _ = send_html(&bot, chat_id, &html).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_bare_plan_request, is_done_text, should_rate_limit, should_start_planning_interview,
        stream_preview, tool_result_html,
    };

    #[test]
    fn stream_preview_truncates_long_output() {
        let long = "a".repeat(5000);
        let preview = stream_preview(&long);
        assert!(preview.len() <= 3803);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn tool_result_html_strips_prefix_and_escapes() {
        let html = tool_result_html("shell", "[tool result]\n<ok>", true);
        assert!(html.contains("<b>Behind the scenes — shell</b>"));
        assert!(html.contains("&lt;ok&gt;"));
        assert!(!html.contains("[tool result]"));
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
}

// ── Message handler ──────────────────────────────────────────────────

/// Handle an incoming message (text, voice, or photo).
async fn handle_message(bot: Bot, msg: Message, state: TelegramState) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    let Some(text) = preflight_message(&bot, &msg, chat_id, &state).await? else {
        return Ok(());
    };

    if handle_slash_commands(&bot, &msg, chat_id, &text, &state).await? {
        return Ok(());
    }

    let planning_active = {
        let planning = state.planning.lock().await;
        planning.contains_key(&chat_id.0)
    };

    if !planning_active && !check_rate_limit(&state, &bot, chat_id).await? {
        return Ok(());
    }

    if handle_planning_message(&bot, &msg, chat_id, &text, &state).await? {
        return Ok(());
    }

    let session = telegram_session(&msg, chat_id);
    dispatch_to_core(
        bot,
        chat_id,
        state,
        session,
        text,
        "<i>Status: Starting...</i>",
    )
    .await
}

async fn preflight_message(
    bot: &Bot,
    msg: &Message,
    chat_id: ChatId,
    state: &TelegramState,
) -> ResponseResult<Option<String>> {
    if !ensure_authorized(bot, chat_id, state).await? {
        return Ok(None);
    }

    let Some(text) = extract_message_text(bot, msg, chat_id, &state.config).await? else {
        return Ok(None);
    };

    if handle_slash_commands(bot, msg, chat_id, &text, state).await? {
        return Ok(None);
    }

    if !check_rate_limit(state, bot, chat_id).await? {
        return Ok(None);
    }

    Ok(Some(text))
}

async fn ensure_authorized(
    bot: &Bot,
    chat_id: ChatId,
    state: &TelegramState,
) -> ResponseResult<bool> {
    // C2: Auth check using allow_all
    if !is_authorized(chat_id.0, &state.config) {
        bot.send_message(chat_id, "<i>Unauthorized.</i>")
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(false);
    }
    Ok(true)
}

async fn send_working_status(bot: &Bot, chat_id: ChatId) -> ResponseResult<Message> {
    tracing::debug!("Sending Working... status message");
    let status_msg = bot
        .send_message(chat_id, "<i>Working...</i>")
        .parse_mode(ParseMode::Html)
        .await?;
    tracing::debug!(msg_id = %status_msg.id, "Status message sent");
    Ok(status_msg)
}

// ── Callback handler ─────────────────────────────────────────────────

/// Handle callback queries (confirmation button presses).
async fn handle_callback(bot: Bot, q: CallbackQuery, state: TelegramState) -> ResponseResult<()> {
    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };

    // Parse callback data by prefix
    let parts: Vec<&str> = data.splitn(3, ':').collect();

    // Handle "cli:<tool_id>" callbacks
    if parts.len() == 2 && parts[0] == "cli" {
        let tool_id = parts[1];
        let result = {
            let mut k = state.handle.knobs.write().unwrap();
            k.set("cli_tool", tool_id)
        };
        match result {
            Ok(_) => {
                state
                    .handle
                    .observer
                    .emit(crate::observer::ObserverEvent::new(
                        ObserverCategory::KnobChange,
                        format!("cli_tool = {}", tool_id),
                    ));
                let label = cli_display_name(tool_id);
                let keyboard = build_cli_keyboard(tool_id);
                let html = format!("<b>Coding CLI tool:</b> {}", escape_html(&label));
                if let Some(msg) = &q.message {
                    if let Some(regular) = msg.regular_message() {
                        let _ = bot
                            .edit_message_text(regular.chat.id, regular.id, &html)
                            .parse_mode(ParseMode::Html)
                            .reply_markup(keyboard)
                            .await;
                    }
                }
                bot.answer_callback_query(&q.id)
                    .text(format!("Switched to {}", label))
                    .await?;
            }
            Err(e) => {
                bot.answer_callback_query(&q.id).text(e).await?;
            }
        }
        return Ok(());
    }

    // Handle "plan:<kind>:<value>" callbacks
    if parts.len() == 3 && parts[0] == "plan" {
        let kind = parts[1];
        let value = parts[2];

        let Some(msg) = &q.message else {
            bot.answer_callback_query(&q.id)
                .text("Session expired, please retry.")
                .await?;
            return Ok(());
        };
        let Some(regular) = msg.regular_message() else {
            bot.answer_callback_query(&q.id)
                .text("Session expired, please retry.")
                .await?;
            return Ok(());
        };
        let chat_id = regular.chat.id;

        enum Action {
            None,
            Prompt(String, Option<InlineKeyboardMarkup>),
            Dispatch(String),
            Cancelled,
        }

        let mut action = Action::None;
        let mut callback_text: Option<String> = None;

        {
            let mut planning = state.planning.lock().await;
            let Some(interview) = planning.get_mut(&chat_id.0) else {
                bot.answer_callback_query(&q.id)
                    .text("Session expired, please retry.")
                    .await?;
                return Ok(());
            };

            interview.last_updated = tokio::time::Instant::now();

            if kind == "confirm" {
                match value {
                    "yes" => {
                        let prompt = planning_build_prompt(interview);
                        interview.step = PlanningStep::Refining;
                        action = Action::Dispatch(prompt);
                        callback_text = Some("Confirmed".to_string());
                    }
                    "edit" => {
                        interview.step = PlanningStep::Editing;
                        action = Action::Prompt(
                            "Send corrections using lines like:\n<code>Goal: ...</code>\n<code>Constraints: ...</code>\n<code>Timeline: ...</code>\n<code>Scope: ...</code>\n<code>Depth: ...</code>\n<code>Output: ...</code>"
                                .to_string(),
                            None,
                        );
                        callback_text = Some("Edit mode".to_string());
                    }
                    "cancel" => {
                        planning.remove(&chat_id.0);
                        action = Action::Cancelled;
                        callback_text = Some("Cancelled".to_string());
                    }
                    _ => {}
                }
            } else if let Some(label) = planning_value_label(kind, value) {
                match kind {
                    "depth" => {
                        interview.depth = Some(label.to_string());
                        callback_text = Some(format!("Depth: {}", label));
                    }
                    "timeline" => {
                        interview.timeline = Some(label.to_string());
                        callback_text = Some(format!("Timeline: {}", label));
                    }
                    "scope" => {
                        interview.scope = Some(label.to_string());
                        callback_text = Some(format!("Scope: {}", label));
                    }
                    "constraints" => {
                        interview.constraints = Some(label.to_string());
                        callback_text = Some("Constraints: none".to_string());
                    }
                    "output" => {
                        interview.output = Some(label.to_string());
                        callback_text = Some(format!("Output: {}", label));
                    }
                    _ => {}
                }

                match interview.step {
                    PlanningStep::Constraints if matches!(kind, "timeline" | "scope" | "constraints") => {
                        interview.step = PlanningStep::Output;
                        action = Action::Prompt(
                            "What format do you want the plan in: checklist, spec, or draft?"
                                .to_string(),
                            Some(planning_output_keyboard()),
                        );
                    }
                    PlanningStep::Output if kind == "output" => {
                        interview.step = PlanningStep::Summary;
                        action =
                            Action::Prompt(planning_summary_html(interview), Some(planning_confirm_keyboard()));
                    }
                    _ => {}
                }
            } else if kind == "skip" {
                match value {
                    "constraints" => {
                        interview.constraints = Some("none".to_string());
                        interview.step = PlanningStep::Output;
                        action = Action::Prompt(
                            "What format do you want the plan in: checklist, spec, or draft?"
                                .to_string(),
                            Some(planning_output_keyboard()),
                        );
                        callback_text = Some("Skipped constraints".to_string());
                    }
                    "output" => {
                        interview.output = Some("checklist".to_string());
                        interview.step = PlanningStep::Summary;
                        action =
                            Action::Prompt(planning_summary_html(interview), Some(planning_confirm_keyboard()));
                        callback_text = Some("Using checklist format".to_string());
                    }
                    _ => {}
                }
            }
        }

        if let Some(text) = callback_text {
            bot.answer_callback_query(&q.id).text(text).await?;
        } else {
            bot.answer_callback_query(&q.id).await?;
        }

        match action {
            Action::None => {}
            Action::Cancelled => {
                send_html(&bot, chat_id, "<i>Planning cancelled.</i>").await?;
            }
            Action::Prompt(html, keyboard) => {
                send_planning_prompt(&bot, chat_id, html, keyboard).await?;
            }
            Action::Dispatch(prompt) => {
                let session = telegram_session_from_user(q.from.id.0.to_string(), chat_id);
                dispatch_to_core_with_followup(
                    bot.clone(),
                    chat_id,
                    state.clone(),
                    session,
                    prompt,
                    "<i>Status: Drafting plan…</i>",
                    Some((
                        "<b>Plan generated.</b> What would you like to do next?".to_string(),
                        planning_post_generate_keyboard(),
                    )),
                )
                .await?;
            }
        }

        return Ok(());
    }

    // Handle post-generation planning actions (Implement / Refine / Done)
    if parts.len() == 3 && parts[0] == "plan" && parts[1] == "post" {
        let chat_id = q.message.as_ref().map(|m| m.chat().id).unwrap_or(ChatId(0));
        match parts[2] {
            "implement" => {
                bot.answer_callback_query(&q.id).text("Starting implementation").await?;
                {
                    let mut planning = state.planning.lock().await;
                    planning.remove(&chat_id.0);
                }
                let session = telegram_session_from_user(q.from.id.0.to_string(), chat_id);
                dispatch_to_core(
                    bot.clone(),
                    chat_id,
                    state.clone(),
                    session,
                    "Implement the plan above step by step. Start with the first actionable item.".to_string(),
                    "<i>Status: Implementing plan…</i>",
                )
                .await?;
            }
            "refine" => {
                bot.answer_callback_query(&q.id).text("Send your refinements").await?;
                send_html(&bot, chat_id, "<i>Send me what you'd like to change and I'll update the plan.</i>").await?;
            }
            "done" => {
                bot.answer_callback_query(&q.id).text("Done").await?;
                {
                    let mut planning = state.planning.lock().await;
                    planning.remove(&chat_id.0);
                }
                send_html(&bot, chat_id, "<i>Plan finalised.</i>").await?;
            }
            _ => {
                bot.answer_callback_query(&q.id).await?;
            }
        }
        return Ok(());
    }

    // Handle "confirm:<id>:<yes|no>" callbacks
    if parts.len() != 3 || parts[0] != "confirm" {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }

    let confirm_id = parts[1];
    let approved = parts[2] == "yes";

    let mut pending = state.pending.lock().await;
    if let Some(tx) = pending.remove(confirm_id) {
        let _ = tx.send(approved);
        let answer_html = if approved {
            "<b>Approved</b>"
        } else {
            "<b>Denied</b>"
        };

        // Update the keyboard message
        if let Some(msg) = &q.message {
            if let Some(regular) = msg.regular_message() {
                let original = regular.text().unwrap_or("");
                let _ = bot
                    .edit_message_text(
                        regular.chat.id,
                        regular.id,
                        format!("{}\n\n{}", escape_html(original), answer_html),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
        }

        let answer = if approved { "Approved" } else { "Denied" };
        bot.answer_callback_query(&q.id).text(answer).await?;
    } else {
        bot.answer_callback_query(&q.id)
            .text("Session expired, please retry.")
            .await?;
    }

    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────

/// Entry point: run the Telegram bot.
pub async fn run_telegram(
    handle: CoreHandle,
    config: TelegramConfig,
    system_info: SystemInfo,
) -> anyhow::Result<()> {
    let token = config
        .token
        .clone()
        .or_else(|| std::env::var("ATHENA_TELEGRAM_TOKEN").ok())
        .ok_or_else(|| anyhow::anyhow!(
            "Telegram token not set. Set [telegram].token in config.toml or ATHENA_TELEGRAM_TOKEN env var"
        ))?;

    let bot = Bot::new(&token);

    // C2: Enforce allow_all — refuse to start if no allowed_chats and allow_all is false
    if config.allowed_chats.is_empty() && !config.allow_all {
        return Err(anyhow::anyhow!(
            "Telegram bot refused to start: allowed_chats is empty and allow_all is false. \
             Set allowed_chats to specific chat IDs or set allow_all = true in [telegram] config."
        ));
    }

    if config.allow_all {
        tracing::warn!("allow_all is true — bot will respond to ALL chats");
    }

    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let last_request: Arc<Mutex<HashMap<i64, tokio::time::Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let planning: Arc<Mutex<HashMap<i64, PlanningInterview>>> = Arc::new(Mutex::new(HashMap::new()));

    // Spawn stale confirmation cleanup task
    let cleanup_pending = pending.clone();
    let cleanup_timeout = config.confirm_timeout_secs;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let mut map = cleanup_pending.lock().await;
            if !map.is_empty() {
                tracing::debug!(
                    count = map.len(),
                    timeout_secs = cleanup_timeout,
                    "Pending confirmations"
                );
                // Drop entries whose receivers have been dropped (task timed out)
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
            let mut map = cleanup_planning.lock().await;
            if !map.is_empty() {
                map.retain(|_, interview| now.duration_since(interview.last_updated) < planning_timeout);
            }
        }
    });

    let state = TelegramState {
        handle,
        pending,
        config,
        last_request,
        planning,
        system_info,
    };

    // Register bot commands menu (the "/" button in Telegram)
    let commands = vec![
        BotCommand::new("help", "Show available commands"),
        BotCommand::new("plan", "Start a planning interview"),
        BotCommand::new("status", "System status & uptime"),
        BotCommand::new("model", "Show/switch LLM model"),
        BotCommand::new("models", "List available models"),
        BotCommand::new("cli_model", "Show/switch CLI tool model"),
        BotCommand::new("knobs", "Runtime knob values"),
        BotCommand::new("mood", "Current mood state"),
        BotCommand::new("jobs", "Scheduled cron jobs"),
        BotCommand::new("session", "Current session info"),
        BotCommand::new("set", "Modify a runtime knob"),
        BotCommand::new("cli", "Switch coding CLI tool"),
        BotCommand::new("ghosts", "List active ghosts"),
        BotCommand::new("memories", "List saved memories"),
        BotCommand::new("dispatch", "Dispatch autonomous task to ghost"),
    ];
    bot.set_my_commands(commands)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to set bot commands: {}", e))?;

    // Spawn pulse delivery task — forwards background pulses to Telegram chats
    {
        let bot_for_pulses = bot.clone();
        let delivered_rx = state.handle.delivered_rx.clone();
        let allowed_chats = state.config.allowed_chats.clone();
        tokio::spawn(async move {
            let mut rx = delivered_rx.lock().await;
            while let Some(pulse) = rx.recv().await {
                let label = match pulse.source.label() {
                    "heartbeat" => "💓 heartbeat",
                    "memory_scan" => "🔍 insight",
                    "idle_musing" => "💭 thought",
                    "conversation_reentry" => "🔄 follow-up",
                    "mood_shift" => "🎭 mood",
                    "autonomous_task" => "⚡ task result",
                    other => other,
                };
                let html = format!(
                    "<b>{}</b>\n{}",
                    escape_html(label),
                    escape_html(&pulse.content)
                );
                // Targeted delivery: send to specific chat or broadcast to all
                let target_chats: Vec<i64> = match &pulse.target {
                    crate::pulse::PulseTarget::Session(ctx) if ctx.platform == "telegram" => {
                        ctx.chat_id.parse::<i64>().ok().into_iter().collect()
                    }
                    _ => allowed_chats.clone(),
                };
                for chat_id in &target_chats {
                    let _ = bot_for_pulses
                        .send_message(ChatId(*chat_id), &html)
                        .parse_mode(ParseMode::Html)
                        .await;
                }
            }
        });
    }

    eprintln!("Telegram bot starting...");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint({
            let state = state.clone();
            move |bot: Bot, msg: Message| {
                let state = state.clone();
                async move { handle_message(bot, msg, state).await }
            }
        }))
        .branch(Update::filter_callback_query().endpoint({
            let state = state.clone();
            move |bot: Bot, q: CallbackQuery| {
                let state = state.clone();
                async move { handle_callback(bot, q, state).await }
            }
        }));

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
