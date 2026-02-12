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

// ── Message handler ──────────────────────────────────────────────────

/// Handle an incoming text message.
async fn handle_message(bot: Bot, msg: Message, state: TelegramState) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t.to_string(),
        None => return Ok(()),
    };

    let chat_id = msg.chat.id;

    // C2: Auth check using allow_all
    if !is_authorized(chat_id.0, &state.config) {
        bot.send_message(chat_id, "<i>Unauthorized.</i>")
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    // ── Slash commands (no rate limit) ───────────────────────────────

    if text == "/start" || text == "/help" {
        let help = "<b>Athena</b>\n\n\
            <b>Commands:</b>\n\
            /status — System status &amp; uptime\n\
            /knobs — Runtime knob values\n\
            /mood — Current mood state\n\
            /jobs — Scheduled cron jobs\n\
            /session — Current session info\n\
            /set <code>&lt;key&gt; &lt;value&gt;</code> — Modify a runtime knob\n\
            /ghosts — List active ghosts\n\
            /memories — List saved memories\n\
            /help — This help message\n\n\
            Send any message to chat with Athena.";
        send_html(&bot, chat_id, help).await?;
        return Ok(());
    }

    if text == "/ghosts" {
        let ghosts = state.handle.list_ghosts();
        let mut out = String::from("<b>Active ghosts:</b>\n\n");
        for g in &ghosts {
            out.push_str(&format!(
                "• <b>{}</b> — {} <code>[{}]</code>\n",
                escape_html(&g.name),
                escape_html(&g.description),
                escape_html(&g.tools.join(", "))
            ));
        }
        send_html(&bot, chat_id, &out).await?;
        return Ok(());
    }

    if text == "/memories" {
        match state.handle.list_memories() {
            Ok(memories) if memories.is_empty() => {
                send_html(&bot, chat_id, "<i>No memories.</i>").await?;
            }
            Ok(memories) => {
                let mut out = String::from("<b>Memories:</b>\n\n");
                for m in &memories {
                    out.push_str(&format!(
                        "<code>[{}]</code> <b>{}</b> — {}\n",
                        escape_html(&m.id[..8]),
                        escape_html(&m.category),
                        escape_html(&m.content)
                    ));
                }
                send_html(&bot, chat_id, &out).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to list memories");
                send_html(&bot, chat_id, "<i>An internal error occurred.</i>").await?;
            }
        }
        return Ok(());
    }

    if text == "/status" {
        let info = &state.system_info;
        let uptime_secs = info.started_at.elapsed().as_secs();
        let ghost_count = state.handle.list_ghosts().len();
        let mood_desc = state.handle.mood.describe();
        let idle = state.handle.activity.idle_secs();
        let last_active = if idle < 5 { "just now".to_string() } else { format!("{} ago", format_duration(idle)) };

        let html = format!(
            "<b>System Status</b>\n\n\
             <b>Provider:</b> <code>{}</code>\n\
             <b>Model:</b> <code>{}</code>\n\
             <b>Temperature:</b> <code>{}</code>\n\
             <b>Max tokens:</b> <code>{}</code>\n\
             <b>Uptime:</b> <code>{}</code>\n\
             <b>Last active:</b> <code>{}</code>\n\
             <b>Ghosts:</b> <code>{}</code>\n\
             <b>Mood:</b> {}",
            escape_html(&info.provider),
            escape_html(&info.model),
            info.temperature,
            info.max_tokens,
            format_duration(uptime_secs),
            escape_html(&last_active),
            ghost_count,
            escape_html(&mood_desc),
        );
        send_html(&bot, chat_id, &html).await?;
        return Ok(());
    }

    if text == "/knobs" {
        let display = {
            let k = state.handle.knobs.read().unwrap();
            k.display()
        };
        let html = format!("<pre>{}</pre>", escape_html(&display));
        send_html(&bot, chat_id, &html).await?;
        return Ok(());
    }

    if text == "/mood" {
        let desc = state.handle.mood.describe();
        let energy = state.handle.mood.energy();
        let modifier = state.handle.mood.modifier();

        let html = format!(
            "<b>Mood</b>\n\n\
             {}\n\
             <b>Energy:</b> <code>{} {:.0}%</code>\n\
             <b>Modifier:</b> <code>{}</code>",
            escape_html(&desc),
            energy_bar(energy),
            energy * 100.0,
            escape_html(&modifier),
        );
        send_html(&bot, chat_id, &html).await?;
        return Ok(());
    }

    if text == "/jobs" {
        if let Some(engine) = &state.handle.cron_engine {
            match engine.list_jobs() {
                Ok(jobs) if jobs.is_empty() => {
                    send_html(&bot, chat_id, "<i>No scheduled jobs.</i>").await?;
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
                    send_html(&bot, chat_id, &out).await?;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to list jobs");
                    send_html(&bot, chat_id, "<i>Failed to list jobs.</i>").await?;
                }
            }
        } else {
            send_html(&bot, chat_id, "<i>Cron engine not initialized.</i>").await?;
        }
        return Ok(());
    }

    if text == "/session" {
        let user_id = msg
            .from
            .as_ref()
            .map(|u| u.id.0.to_string())
            .unwrap_or_else(|| "unknown".into());
        let session_key = format!("telegram:{}:{}", user_id, chat_id.0);

        let turns = state
            .handle
            .memory
            .recent_turns(&session_key, 20)
            .unwrap_or_default();

        let turn_count = turns.len();
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
            "<b>Session</b>\n\n\
             <b>Key:</b> <code>{}</code>\n\
             <b>Recent turns:</b> <code>{}</code>\n\
             <b>Last message:</b>\n<i>{}</i>",
            escape_html(&session_key),
            turn_count,
            escape_html(&last_preview),
        );
        send_html(&bot, chat_id, &html).await?;
        return Ok(());
    }

    if text.starts_with("/set") {
        let parts: Vec<&str> = text.split_whitespace().collect();
        match parts.len() {
            1 => {
                // /set — show all knobs (same as /knobs)
                let display = {
                    let k = state.handle.knobs.read().unwrap();
                    k.display()
                };
                let html = format!("<pre>{}</pre>", escape_html(&display));
                send_html(&bot, chat_id, &html).await?;
            }
            3 => {
                // /set key value — drop guard before any .await
                let result = {
                    let mut k = state.handle.knobs.write().unwrap();
                    k.set(parts[1], parts[2])
                };
                match result {
                    Ok(msg) => {
                        state.handle.observer.emit(crate::observer::ObserverEvent::new(
                            ObserverCategory::KnobChange,
                            format!("{} = {}", parts[1], parts[2]),
                        ));
                        let html = format!("<b>Set:</b> {}", escape_html(&msg));
                        send_html(&bot, chat_id, &html).await?;
                    }
                    Err(e) => {
                        let html = format!("<i>Error: {}</i>", escape_html(&e));
                        send_html(&bot, chat_id, &html).await?;
                    }
                }
            }
            _ => {
                send_html(&bot, chat_id, "<i>Usage: /set or /set &lt;key&gt; &lt;value&gt;</i>").await?;
            }
        }
        return Ok(());
    }

    // ── Rate-limited LLM messages ────────────────────────────────────

    // M7: Per-chat rate limiting (5-second cooldown before LLM-invoking messages)
    {
        let mut last_req = state.last_request.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(last) = last_req.get(&chat_id.0) {
            if now.duration_since(*last) < tokio::time::Duration::from_secs(5) {
                send_html(&bot, chat_id, "<i>Please wait a few seconds before sending another request.</i>").await?;
                return Ok(());
            }
        }
        last_req.insert(chat_id.0, now);
    }

    // Build confirmer for this chat
    let confirmer: Arc<dyn Confirmer> = Arc::new(TelegramConfirmer {
        bot: bot.clone(),
        chat_id,
        pending: state.pending.clone(),
        timeout_secs: state.config.confirm_timeout_secs,
    });

    let session = SessionContext {
        platform: "telegram".into(),
        user_id: msg
            .from
            .as_ref()
            .map(|u| u.id.0.to_string())
            .unwrap_or_else(|| "unknown".into()),
        chat_id: chat_id.0.to_string(),
    };

    tracing::debug!("Sending Working... status message");
    // Send "Working..." status message that we'll update
    let status_msg = bot
        .send_message(chat_id, "<i>Working...</i>")
        .parse_mode(ParseMode::Html)
        .await?;
    tracing::debug!(msg_id = %status_msg.id, "Status message sent");

    tracing::debug!("Sending to core via handle.chat()");
    let mut events = match state.handle.chat(session, &text, confirmer).await {
        Ok(rx) => rx,
        Err(e) => {
            // M2: Log details, send generic message to user
            tracing::error!(error = %e, "handle.chat() failed");
            let _ = bot
                .edit_message_text(chat_id, status_msg.id, "<i>An error occurred while processing your request.</i>")
                .parse_mode(ParseMode::Html)
                .await;
            return Ok(());
        }
    };
    tracing::debug!("Waiting for core events");

    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Status(s) => {
                let _ = bot
                    .edit_message_text(chat_id, status_msg.id, format!("<i>{}</i>", escape_html(&s)))
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            CoreEvent::Response(r) => {
                // Delete the status message
                let _ = bot.delete_message(chat_id, status_msg.id).await;

                // Send response, escaped and chunked
                let escaped = escape_html(&r);
                for chunk in chunk_message(&escaped, 4000) {
                    bot.send_message(chat_id, chunk)
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
            }
            CoreEvent::Error(e) => {
                // M2: Log details, send generic message to user
                tracing::error!(error = %e, chat_id = chat_id.0, "Task error");
                let _ = bot
                    .edit_message_text(
                        chat_id,
                        status_msg.id,
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

    Ok(())
}

// ── Callback handler ─────────────────────────────────────────────────

/// Handle callback queries (confirmation button presses).
async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: TelegramState,
) -> ResponseResult<()> {
    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };

    // Parse "confirm:<id>:<yes|no>"
    let parts: Vec<&str> = data.splitn(3, ':').collect();
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
        bot.answer_callback_query(&q.id)
            .text(answer)
            .await?;
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

    let state = TelegramState {
        handle,
        pending,
        config,
        last_request,
        system_info,
    };

    // Register bot commands menu (the "/" button in Telegram)
    let commands = vec![
        BotCommand::new("help", "Show available commands"),
        BotCommand::new("status", "System status & uptime"),
        BotCommand::new("knobs", "Runtime knob values"),
        BotCommand::new("mood", "Current mood state"),
        BotCommand::new("jobs", "Scheduled cron jobs"),
        BotCommand::new("session", "Current session info"),
        BotCommand::new("set", "Modify a runtime knob"),
        BotCommand::new("ghosts", "List active ghosts"),
        BotCommand::new("memories", "List saved memories"),
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
                    other => other,
                };
                let html = format!(
                    "<b>{}</b>\n{}",
                    escape_html(label),
                    escape_html(&pulse.content)
                );
                for &chat_id in &allowed_chats {
                    let _ = bot_for_pulses
                        .send_message(ChatId(chat_id), &html)
                        .parse_mode(ParseMode::Html)
                        .await;
                }
            }
        });
    }

    eprintln!("Telegram bot starting...");

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .endpoint({
                    let state = state.clone();
                    move |bot: Bot, msg: Message| {
                        let state = state.clone();
                        async move { handle_message(bot, msg, state).await }
                    }
                }),
        )
        .branch(
            Update::filter_callback_query()
                .endpoint({
                    let state = state.clone();
                    move |bot: Bot, q: CallbackQuery| {
                        let state = state.clone();
                        async move { handle_callback(bot, q, state).await }
                    }
                }),
        );

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
