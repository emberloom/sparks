use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use axum::{
    extract::{rejection::JsonRejection, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::OpenAiApiConfig;
use crate::confirm::AutoConfirmer;
use crate::core::{CoreEvent, CoreHandle, SessionContext};
use crate::observer::{ObserverCategory, ObserverHandle};

const RATE_WINDOW_SECS: u64 = 60;
const CHAT_TIMEOUT_SECS: u64 = 300;

#[derive(Clone)]
struct OpenAiApiState {
    core: CoreHandle,
    observer: ObserverHandle,
    api_key: String,
    principal: String,
    models: Arc<Vec<String>>,
    models_lookup: Arc<HashSet<String>>,
    limiter: InMemoryRateLimiter,
    started_at: i64,
}

#[derive(Clone)]
struct InMemoryRateLimiter {
    limit: usize,
    window: Duration,
    hits: Arc<tokio::sync::Mutex<VecDeque<Instant>>>,
}

impl InMemoryRateLimiter {
    fn new(requests_per_minute: u32, burst: u32) -> Self {
        let limit = requests_per_minute as usize + burst as usize;
        Self {
            limit,
            window: Duration::from_secs(RATE_WINDOW_SECS),
            hits: Arc::new(tokio::sync::Mutex::new(VecDeque::new())),
        }
    }

    async fn allow(&self) -> bool {
        if self.limit == 0 {
            return true;
        }

        let now = Instant::now();
        let mut hits = self.hits.lock().await;
        while let Some(front) = hits.front().copied() {
            if now.duration_since(front) < self.window {
                break;
            }
            let _ = hits.pop_front();
        }
        if hits.len() >= self.limit {
            return false;
        }
        hits.push_back(now);
        true
    }
}

pub async fn spawn_openai_api(config: OpenAiApiConfig, core: CoreHandle) -> anyhow::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    let api_key = resolve_api_key(&config).ok_or_else(|| {
        anyhow!(
            "OpenAI-compatible API enabled but no API key configured (set {} or openai_api.api_key)",
            config.api_key_env
        )
    })?;
    let models = derive_model_ids(&config, &core);
    let models_lookup = models.iter().cloned().collect::<HashSet<_>>();
    let observer = core.observer.clone();
    let bind = config.bind.clone();
    let state = OpenAiApiState {
        core,
        observer: observer.clone(),
        api_key,
        principal: sanitize_segment(&config.principal, "self"),
        models: Arc::new(models),
        models_lookup: Arc::new(models_lookup),
        limiter: InMemoryRateLimiter::new(config.requests_per_minute, config.burst),
        started_at: Utc::now().timestamp(),
    };

    let app = Router::new()
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("Failed to bind OpenAI-compatible API on {}", bind))?;

    observer.log(
        ObserverCategory::Startup,
        format!("OpenAI-compatible API bound on {}", bind),
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("OpenAI-compatible API server error: {}", e);
            observer.log(
                ObserverCategory::Startup,
                format!("OpenAI-compatible API server error: {}", e),
            );
        }
    });

    Ok(())
}

fn resolve_api_key(config: &OpenAiApiConfig) -> Option<String> {
    if let Some(key) = config.api_key.as_ref() {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    std::env::var(&config.api_key_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn derive_model_ids(config: &OpenAiApiConfig, core: &CoreHandle) -> Vec<String> {
    if !config.advertised_models.is_empty() {
        return dedup_model_ids(config.advertised_models.iter().map(String::as_str));
    }

    let mut ids = vec!["athena".to_string()];
    let current_model = core.llm.current_model();
    if !current_model.trim().is_empty() {
        ids.push(current_model);
    }

    for ghost in core.list_ghosts() {
        ids.push(format!("athena/{}", ghost.name));
    }

    dedup_model_ids(ids.iter().map(String::as_str))
}

fn dedup_model_ids<'a>(models: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for model in models {
        let normalized = model.trim();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_string()) {
            out.push(normalized.to_string());
        }
    }
    if out.is_empty() {
        out.push("athena".to_string());
    }
    out
}

async fn handle_models(State(state): State<OpenAiApiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize_and_rate_limit(&state, &headers).await {
        return resp;
    }

    state
        .observer
        .log(ObserverCategory::ChatIn, "openai_api /v1/models accepted");

    let data = state
        .models
        .iter()
        .map(|id| ModelInfo {
            id: id.clone(),
            object: "model",
            created: state.started_at,
            owned_by: "athena",
            permission: Vec::new(),
        })
        .collect::<Vec<_>>();

    state.observer.log(
        ObserverCategory::ChatOut,
        format!("openai_api /v1/models completed ({} model(s))", data.len()),
    );

    (
        StatusCode::OK,
        Json(ModelsResponse {
            object: "list",
            data,
        }),
    )
        .into_response()
}

async fn handle_chat_completions(
    State(state): State<OpenAiApiState>,
    headers: HeaderMap,
    payload: Result<Json<ChatCompletionsRequest>, JsonRejection>,
) -> Response {
    if let Err(resp) = authorize_and_rate_limit(&state, &headers).await {
        return resp;
    }

    let Json(req) = match payload {
        Ok(body) => body,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON body: {}", e),
                "invalid_request_error",
                Some("invalid_json"),
            );
        }
    };

    let requested_model = req.model.trim().to_string();
    if requested_model.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "Field 'model' is required.",
            "invalid_request_error",
            Some("model_required"),
        );
    }
    if !state.models_lookup.contains(&requested_model) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            format!("Model '{}' is not available on this Athena instance.", requested_model),
            "invalid_request_error",
            Some("model_not_found"),
        );
    }
    if req.stream.unwrap_or(false) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "stream=true is not supported by Athena's OpenAI-compatible API.",
            "invalid_request_error",
            Some("stream_not_supported"),
        );
    }
    if let Some(temp) = req.temperature {
        if !temp.is_finite() {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "temperature must be a finite number when provided.",
                "invalid_request_error",
                Some("invalid_temperature"),
            );
        }
    }

    let prompt = match normalize_messages(&req.messages) {
        Ok(text) => text,
        Err(message) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                message,
                "invalid_request_error",
                Some("invalid_messages"),
            );
        }
    };

    let chat_id = sanitize_segment(req.user.as_deref().unwrap_or("default"), "default");
    let session = SessionContext {
        platform: "openai_api".to_string(),
        user_id: state.principal.clone(),
        chat_id: chat_id.clone(),
    };
    state.observer.log(
        ObserverCategory::ChatIn,
        format!(
            "openai_api /v1/chat/completions accepted (session={}, model={})",
            chat_id, requested_model
        ),
    );

    let confirmer: Arc<dyn crate::confirm::Confirmer> = Arc::new(AutoConfirmer);
    let events = match state.core.chat(session, &prompt, confirmer).await {
        Ok(rx) => rx,
        Err(e) => {
            state.observer.log(
                ObserverCategory::ChatOut,
                format!("openai_api dispatch failed: {}", e),
            );
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to dispatch request to Athena core.",
                "server_error",
                Some("dispatch_failed"),
            );
        }
    };

    let response_text = match tokio::time::timeout(
        Duration::from_secs(CHAT_TIMEOUT_SECS),
        await_final_response(events),
    )
    .await
    {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            state.observer.log(
                ObserverCategory::ChatOut,
                format!("openai_api request errored: {}", e),
            );
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Athena core failed to complete request: {}", e),
                "server_error",
                Some("core_error"),
            );
        }
        Err(_) => {
            state.observer.log(
                ObserverCategory::ChatOut,
                "openai_api request timed out".to_string(),
            );
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Athena core timed out while completing the request.",
                "server_error",
                Some("timeout"),
            );
        }
    };

    state.observer.log(
        ObserverCategory::ChatOut,
        format!(
            "openai_api /v1/chat/completions completed (session={}, model={})",
            chat_id, requested_model
        ),
    );

    (
        StatusCode::OK,
        Json(build_chat_completion_response(
            requested_model,
            response_text,
        )),
    )
        .into_response()
}

async fn authorize_and_rate_limit(
    state: &OpenAiApiState,
    headers: &HeaderMap,
) -> std::result::Result<(), Response> {
    if !is_authorized(headers, &state.api_key) {
        return Err(openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid API key.",
            "authentication_error",
            Some("invalid_api_key"),
        ));
    }

    if !state.limiter.allow().await {
        return Err(openai_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded for this Athena instance.",
            "rate_limit_error",
            Some("rate_limit_exceeded"),
        ));
    }

    Ok(())
}

fn is_authorized(headers: &HeaderMap, expected_api_key: &str) -> bool {
    let Some(raw) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
    else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), expected_api_key.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

async fn await_final_response(mut events: tokio::sync::mpsc::Receiver<CoreEvent>) -> anyhow::Result<String> {
    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Response(text) => return Ok(text),
            CoreEvent::Error(err) => return Err(anyhow!(err)),
            CoreEvent::Status(_) | CoreEvent::StreamChunk(_) | CoreEvent::ToolRun { .. } => {}
        }
    }
    Err(anyhow!("Athena core closed event stream without a final response"))
}

fn normalize_messages(messages: &[ChatMessageIn]) -> std::result::Result<String, String> {
    if messages.is_empty() {
        return Err("Field 'messages' must contain at least one message.".to_string());
    }

    let mut out = Vec::new();
    for message in messages {
        let role = message.role.trim().to_lowercase();
        if role.is_empty() {
            return Err("Each message must include a non-empty 'role'.".to_string());
        }
        let Some(text) = extract_message_text(&message.content) else {
            return Err(format!(
                "Message content for role '{}' must be a string or text part array.",
                role
            ));
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(format!("{}:\n{}", role, trimmed));
    }

    if out.is_empty() {
        return Err("Messages did not contain any non-empty text content.".to_string());
    }
    Ok(out.join("\n\n"))
}

fn extract_message_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(extract_text_part)
                .collect::<Vec<_>>();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn extract_text_part(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => {
            let text = map.get("text").and_then(Value::as_str)?;
            let part_type = map.get("type").and_then(Value::as_str).unwrap_or("text");
            if part_type == "text" || part_type == "input_text" || part_type == "output_text" {
                Some(text.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn sanitize_segment(input: &str, fallback: &str) -> String {
    let value = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || ['-', '_', '.', ':'].contains(c))
        .take(64)
        .collect::<String>();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn build_chat_completion_response(model: String, content: String) -> ChatCompletionsResponse {
    ChatCompletionsResponse {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        object: "chat.completion",
        created: Utc::now().timestamp(),
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content,
            },
            finish_reason: "stop",
        }],
        usage: ChatUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
    }
}

fn openai_error(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &'static str,
    code: Option<&'static str>,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: OpenAiError {
                message: message.into(),
                error_type,
                code,
            },
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsRequest {
    model: String,
    messages: Vec<ChatMessageIn>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessageIn {
    role: String,
    content: Value,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
struct ModelInfo {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
    permission: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsResponse {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<ChatChoice>,
    usage: ChatUsage,
}

#[derive(Debug, Serialize)]
struct ChatChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct AssistantMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: OpenAiError,
}

#[derive(Debug, Serialize)]
struct OpenAiError {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn extract_message_text_supports_string_and_part_arrays() {
        let plain = Value::String("hello".to_string());
        let plain_text = extract_message_text(&plain);
        assert_eq!(plain_text.as_deref(), Some("hello"));

        let parts = json!([
            {"type":"text","text":"line1"},
            {"type":"input_text","text":"line2"}
        ]);
        let joined = extract_message_text(&parts);
        assert_eq!(joined.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn normalize_messages_rejects_empty_inputs() {
        let input = vec![ChatMessageIn {
            role: "user".to_string(),
            content: Value::String("   ".to_string()),
        }];
        let result = normalize_messages(&input);
        assert!(result.is_err());
    }

    #[test]
    fn auth_accepts_only_valid_bearer_token() {
        let mut headers = HeaderMap::new();
        let inserted = HeaderValue::from_str("Bearer secret-token");
        if let Ok(value) = inserted {
            headers.insert("authorization", value);
        }
        assert!(is_authorized(&headers, "secret-token"));
        assert!(!is_authorized(&headers, "wrong-token"));
    }

    #[tokio::test]
    async fn rate_limiter_enforces_limit_plus_burst() {
        let limiter = InMemoryRateLimiter::new(1, 1);
        assert!(limiter.allow().await);
        assert!(limiter.allow().await);
        assert!(!limiter.allow().await);
    }

    #[test]
    fn response_shape_matches_openai_chat_schema() {
        let response = build_chat_completion_response("athena".to_string(), "done".to_string());
        let json = serde_json::to_value(response);
        assert!(json.is_ok());
        let Some(body) = json.ok() else {
            return;
        };
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["model"], "athena");
        assert_eq!(body["choices"][0]["message"]["role"], "assistant");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert!(body.get("usage").is_some());
    }
}
