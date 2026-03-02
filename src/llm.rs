use std::collections::{HashMap, HashSet};
use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::{OllamaConfig, OuathConfig};
use crate::error::{AthenaError, Result};
use crate::ouath::{OuathAuth, OuathTokens};

// ---------------------------------------------------------------------------
// Token usage tracking
// ---------------------------------------------------------------------------

/// Token counts from a single API call.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Tracks cumulative token budget across an agentic loop.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub context_window: u64,
    // Not used in production logic; verified in tests
    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    pub reserved_for_completion: u64,
    pub last_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub call_count: u32,
}

impl TokenBudget {
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            reserved_for_completion: context_window / 4,
            last_prompt_tokens: 0,
            total_completion_tokens: 0,
            call_count: 0,
        }
    }

    /// Record usage from an API call.
    pub fn record_usage(&mut self, usage: &TokenUsage) {
        self.last_prompt_tokens = usage.prompt_tokens;
        self.total_completion_tokens += usage.completion_tokens;
        self.call_count += 1;
    }

    /// Fraction of context window used by the last prompt.
    pub fn utilization(&self) -> f64 {
        if self.context_window == 0 {
            return 0.0;
        }
        self.last_prompt_tokens as f64 / self.context_window as f64
    }

    /// Returns true when the context is getting full and history should be compressed.
    pub fn needs_compression(&self, threshold: f64) -> bool {
        self.utilization() > threshold
    }
}

// ---------------------------------------------------------------------------
// Streaming types
// ---------------------------------------------------------------------------

/// Events emitted during a streaming LLM response.
#[derive(Debug)]
pub enum StreamEvent {
    /// Incremental text delta from the assistant.
    TextDelta(String),
    /// A fully accumulated tool call (streamed deltas have been assembled).
    ToolCallComplete(ToolCall),
    /// Token usage from the final chunk (when `stream_options.include_usage` is set).
    Usage(TokenUsage),
    /// Stream is complete.
    Done,
}

/// Accumulator for a tool call being streamed in deltas.
#[derive(Debug, Clone, Default)]
struct PartialToolCall {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

/// A single SSE chunk from the streaming API.
#[derive(Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// Native function calling types
// ---------------------------------------------------------------------------

/// Tool definition sent to the API (used in the `tools` parameter).
#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A tool call parsed from the API response.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Response from `chat_with_tools()` — either plain text or structured tool calls.
#[derive(Debug)]
pub enum ChatResponse {
    Text(String),
    ToolCalls {
        tool_calls: Vec<ToolCall>,
        text: Option<String>,
    },
}

/// Extended message type that supports the `tool` role and assistant tool_calls.
#[derive(Debug, Clone)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        content: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    /// Convert to a simple `Message` for fallback to `chat()`.
    pub fn to_simple(&self) -> Option<Message> {
        match self {
            ChatMessage::System(s) => Some(Message::system(s)),
            ChatMessage::User(s) => Some(Message::user(s)),
            ChatMessage::Assistant { content, .. } => {
                content.as_ref().map(|c| Message::assistant(c))
            }
            ChatMessage::Tool { content, .. } => Some(Message::user(content)),
        }
    }
}

// Wire-format structs for OpenAI-compatible API

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ApiFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize)]
struct ApiToolDefinition {
    #[serde(rename = "type")]
    def_type: String,
    function: ApiFunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
struct ApiFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

fn tool_schemas_to_api(tools: &[ToolSchema]) -> Option<Vec<ApiToolDefinition>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|t| ApiToolDefinition {
                def_type: "function".to_string(),
                function: ApiFunctionDefinition {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<String>;
    async fn health_check(&self) -> Result<()>;
    fn provider_name(&self) -> &str;

    /// Whether this provider supports native function calling.
    fn supports_tools(&self) -> bool {
        false
    }

    /// Context window size for this provider/model.
    fn context_window(&self) -> u64 {
        128_000
    }

    /// Chat with native tool definitions. Default: falls back to `chat()`.
    /// Returns the response and optional token usage from the API.
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
    ) -> Result<(ChatResponse, Option<TokenUsage>)> {
        let simple: Vec<Message> = messages.iter().filter_map(|m| m.to_simple()).collect();
        let text = self.chat(&simple).await?;
        Ok((ChatResponse::Text(text), None))
    }

    /// Return the currently active model name.
    fn current_model(&self) -> String {
        String::new()
    }

    /// Override the model at runtime (None = reset to config default).
    fn set_model_override(&self, _model: Option<String>) {}

    /// List models available from this provider's API.
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Query account credits (total, used). Used by the telegram feature.
    #[cfg(feature = "telegram")]
    async fn credits(&self) -> Result<Option<(f64, f64)>> {
        Ok(None)
    }

    /// Whether this provider supports streaming.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Streaming variant of `chat_with_tools()`.
    /// Returns a receiver that yields `StreamEvent`s as the response arrives.
    /// Default: wraps the non-streaming path as a one-shot stream.
    async fn chat_with_tools_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(32);
        let (response, usage) = self.chat_with_tools(messages, tools).await?;

        match response {
            ChatResponse::Text(text) => {
                let _ = tx.send(StreamEvent::TextDelta(text)).await;
            }
            ChatResponse::ToolCalls { tool_calls, text } => {
                if let Some(t) = text {
                    let _ = tx.send(StreamEvent::TextDelta(t)).await;
                }
                for tc in tool_calls {
                    let _ = tx.send(StreamEvent::ToolCallComplete(tc)).await;
                }
            }
        }
        if let Some(u) = usage {
            let _ = tx.send(StreamEvent::Usage(u)).await;
        }
        let _ = tx.send(StreamEvent::Done).await;
        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// Ouath (OpenAI subscription OAuth) client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OuathApiStyle {
    Responses,
    ChatCompletions,
}

pub struct OuathClient {
    client: Client,
    config: OuathConfig,
    auth: OuathAuth,
    model_override: std::sync::RwLock<Option<String>>,
}

impl OuathClient {
    pub fn new(config: OuathConfig) -> Self {
        let auth = OuathAuth::new(config.clone());
        Self {
            client: Client::new(),
            config,
            auth,
            model_override: std::sync::RwLock::new(None),
        }
    }

    fn effective_model(&self) -> String {
        self.model_override
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.config.model.clone())
    }

    fn api_style(&self) -> OuathApiStyle {
        if let Some(style) = self.config.api_style.as_deref() {
            return match style {
                "chat_completions" | "chat" => OuathApiStyle::ChatCompletions,
                _ => OuathApiStyle::Responses,
            };
        }
        let url = self.config.url.as_str();
        if url.contains("/responses") {
            OuathApiStyle::Responses
        } else if url.contains("/chat/completions") || url.contains("api.openai.com") {
            OuathApiStyle::ChatCompletions
        } else {
            OuathApiStyle::Responses
        }
    }

    fn endpoint(&self, style: OuathApiStyle) -> String {
        match style {
            OuathApiStyle::ChatCompletions => {
                if self.config.url.contains("/chat/completions") {
                    self.config.url.clone()
                } else {
                    format!("{}/chat/completions", self.config.url.trim_end_matches('/'))
                }
            }
            OuathApiStyle::Responses => {
                if self.config.url.contains("/responses") {
                    self.config.url.clone()
                } else {
                    format!("{}/responses", self.config.url.trim_end_matches('/'))
                }
            }
        }
    }

    fn requires_account_id(&self) -> bool {
        let url = self.config.url.as_str();
        url.contains("chatgpt.com") || url.contains("backend-api")
    }

    async fn send_with_auth_retry<T: Serialize + ?Sized>(
        &self,
        style: OuathApiStyle,
        body: &T,
        extra_header: Option<(&str, &str)>,
    ) -> Result<(reqwest::Response, Instant)> {
        let mut tokens = self.auth.ensure_valid_tokens().await?;
        let mut start = Instant::now();
        let mut resp = send_ouath_request(
            &self.client,
            &self.endpoint(style),
            body,
            &tokens,
            self.requires_account_id(),
            extra_header,
        )
        .await?;
        if matches!(resp.status().as_u16(), 401 | 403) {
            tokens = self.auth.force_refresh().await?;
            start = Instant::now();
            resp = send_ouath_request(
                &self.client,
                &self.endpoint(style),
                body,
                &tokens,
                self.requires_account_id(),
                extra_header,
            )
            .await?;
        }
        Ok((resp, start))
    }

    async fn chat_with_tools_chat_completions(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<(ChatResponse, Option<TokenUsage>)> {
        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = "OpenAI (Ouath)",
            model = %model,
            messages = messages.len(),
            has_tools,
            "LLM request (with tools)"
        );

        let wire_messages: Vec<Value> = messages
            .iter()
            .filter_map(|m| chat_message_to_wire(m))
            .collect();

        let api_tools = tool_schemas_to_api(tools);

        let req = OpenAiChatRequestWithTools {
            model,
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            tools: api_tools,
            stream: None,
            stream_options: None,
        };

        let (resp, start) = self
            .send_with_auth_retry(OuathApiStyle::ChatCompletions, &req, None)
            .await?;
        let latency = start.elapsed();
        crate::introspect::record_llm_latency(latency.as_millis() as u64);

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "OpenAI (Ouath)", %status, "LLM error");
            crate::introspect::record_error();
            return Err(AthenaError::Llm(format!(
                "Ouath returned {}: {}",
                status, body
            )));
        }

        crate::introspect::record_call();
        let chat_resp: OpenAiChatResponseFull = resp.json().await?;
        let usage = chat_resp.usage.map(|u| TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
        });

        let Some(choice) = chat_resp.choices.into_iter().next() else {
            return Err(AthenaError::Llm("Ouath returned empty choices".into()));
        };

        if let Some(tool_calls) = choice.message.tool_calls {
            let tool_calls: Vec<ToolCall> = tool_calls
                .into_iter()
                .filter_map(|tc| {
                    let args: Value = serde_json::from_str(&tc.function.arguments).ok()?;
                    Some(ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: args,
                    })
                })
                .collect();
            return Ok((
                ChatResponse::ToolCalls {
                    tool_calls,
                    text: choice.message.content,
                },
                usage,
            ));
        }

        Ok((
            ChatResponse::Text(choice.message.content.unwrap_or_default()),
            usage,
        ))
    }

    async fn chat_with_tools_responses(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<(ChatResponse, Option<TokenUsage>)> {
        if self.config.url.contains("chatgpt.com") || self.config.url.contains("backend-api") {
            let rx = self
                .chat_with_tools_stream_responses(messages, tools)
                .await?;
            return collect_stream_response(rx).await;
        }

        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = "OpenAI (Ouath)",
            model = %model,
            messages = messages.len(),
            has_tools,
            "LLM request (responses)"
        );

        let (instructions, input) = build_responses_input(messages);
        let api_tools = tool_schemas_to_api(tools);

        let mut req = serde_json::json!({
            "model": model,
            "input": input,
            "stream": false,
            "store": false,
        });
        if let Some(instr) = instructions {
            req["instructions"] = Value::String(instr);
        }
        if let Some(api_tools) = api_tools {
            req["tools"] = serde_json::to_value(api_tools).unwrap_or(Value::Null);
            req["tool_choice"] = Value::String("auto".into());
            req["parallel_tool_calls"] = Value::Bool(true);
        }
        if !self.config.url.contains("chatgpt.com") && !self.config.url.contains("backend-api") {
            if let Some(temp) = serde_json::Number::from_f64(self.config.temperature as f64) {
                req["temperature"] = Value::Number(temp);
            }
            req["max_output_tokens"] = Value::Number(self.config.max_tokens.into());
        }
        if self.config.reasoning_effort.is_some() || self.config.reasoning_summary.is_some() {
            let mut reasoning = serde_json::Map::new();
            if let Some(ref effort) = self.config.reasoning_effort {
                reasoning.insert("effort".into(), Value::String(effort.clone()));
            }
            if let Some(ref summary) = self.config.reasoning_summary {
                reasoning.insert("summary".into(), Value::String(summary.clone()));
            }
            req["reasoning"] = Value::Object(reasoning);
        }
        if !self.config.include.is_empty() {
            req["include"] = Value::Array(
                self.config
                    .include
                    .iter()
                    .map(|v| Value::String(v.clone()))
                    .collect(),
            );
        }

        let (resp, start) = self
            .send_with_auth_retry(
                OuathApiStyle::Responses,
                &req,
                Some(("OpenAI-Beta", "responses=experimental")),
            )
            .await?;
        let latency = start.elapsed();
        crate::introspect::record_llm_latency(latency.as_millis() as u64);

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "OpenAI (Ouath)", %status, "LLM error");
            crate::introspect::record_error();
            return Err(AthenaError::Llm(format!(
                "Ouath returned {}: {}",
                status, body
            )));
        }

        crate::introspect::record_call();
        let body: Value = resp.json().await?;
        parse_responses_body(&body)
    }
}

#[async_trait]
impl LlmProvider for OuathClient {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        let converted: Vec<ChatMessage> = messages
            .iter()
            .map(|m| match m.role {
                ChatRole::System => ChatMessage::System(m.content.clone()),
                ChatRole::User => ChatMessage::User(m.content.clone()),
                ChatRole::Assistant => ChatMessage::Assistant {
                    content: Some(m.content.clone()),
                    tool_calls: None,
                },
            })
            .collect();

        let (resp, _) = self.chat_with_tools(&converted, &[]).await?;
        Ok(match resp {
            ChatResponse::Text(t) => t,
            ChatResponse::ToolCalls { text, .. } => text.unwrap_or_default(),
        })
    }

    async fn health_check(&self) -> Result<()> {
        self.auth.ensure_valid_tokens().await?;
        Ok(())
    }

    fn provider_name(&self) -> &str {
        "OpenAI (Ouath)"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn context_window(&self) -> u64 {
        self.config.context_window
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<(ChatResponse, Option<TokenUsage>)> {
        match self.api_style() {
            OuathApiStyle::ChatCompletions => {
                self.chat_with_tools_chat_completions(messages, tools).await
            }
            OuathApiStyle::Responses => self.chat_with_tools_responses(messages, tools).await,
        }
    }

    fn current_model(&self) -> String {
        self.effective_model()
    }

    fn set_model_override(&self, model: Option<String>) {
        *self
            .model_override
            .write()
            .unwrap_or_else(|e| e.into_inner()) = model;
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let tokens = self.auth.ensure_valid_tokens().await?;
        let mut builder = self
            .client
            .get(format!("{}/models", self.config.url.trim_end_matches('/')))
            .bearer_auth(&tokens.access_token);
        if let Some(account_id) = &tokens.chatgpt_account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
        let resp = builder.send().await?;
        if !resp.status().is_success() {
            return Ok(vec![self.effective_model()]);
        }
        let body: Value = resp.json().await?;
        let mut models: Vec<String> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if models.is_empty() {
            models.push(self.effective_model());
        }
        models.sort();
        Ok(models)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_tools_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        match self.api_style() {
            OuathApiStyle::ChatCompletions => {
                self.chat_with_tools_stream_chat_completions(messages, tools)
                    .await
            }
            OuathApiStyle::Responses => {
                self.chat_with_tools_stream_responses(messages, tools).await
            }
        }
    }
}

fn build_responses_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for msg in messages {
        match msg {
            ChatMessage::System(s) => instructions.push(s.clone()),
            ChatMessage::User(s) => input.push(serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": s}],
            })),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    input.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                if let Some(calls) = tool_calls {
                    for tc in calls {
                        input.push(serde_json::json!({
                            "type": "tool_call",
                            "id": tc.id,
                            "name": tc.name,
                            "arguments": tc.arguments.to_string(),
                        }));
                    }
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                input.push(serde_json::json!({
                    "type": "tool_output",
                    "tool_call_id": tool_call_id,
                    "output": content,
                }));
            }
        }
    }

    let instructions = if instructions.is_empty() {
        None
    } else {
        Some(instructions.join("\n\n"))
    };
    (instructions, input)
}

impl OuathClient {
    async fn chat_with_tools_stream_chat_completions(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = "OpenAI (Ouath)",
            model = %model,
            messages = messages.len(),
            has_tools,
            stream = true,
            "LLM request (streaming)"
        );

        let wire_messages: Vec<Value> = messages
            .iter()
            .filter_map(|m| chat_message_to_wire(m))
            .collect();

        let api_tools = tool_schemas_to_api(tools);

        let req = OpenAiChatRequestWithTools {
            model,
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            tools: api_tools,
            stream: Some(true),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };

        let (resp, _start) = self
            .send_with_auth_retry(OuathApiStyle::ChatCompletions, &req, None)
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "OpenAI (Ouath)", %status, "LLM stream error");
            return Err(AthenaError::Llm(format!(
                "Ouath returned {}: {}",
                status, body
            )));
        }

        let (tx, rx) = mpsc::channel(64);
        let stream_start = Instant::now();

        tokio::spawn(async move {
            use tokio_stream::StreamExt;

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut partial_calls: Vec<PartialToolCall> = Vec::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(provider = "OpenAI (Ouath)", error = %e, "Stream read error");
                        crate::introspect::record_error();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline_pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=newline_pos).collect();
                    let line = line.trim();

                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }

                    if !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    if data == "[DONE]" {
                        for pc in partial_calls.drain(..) {
                            let args = serde_json::from_str(&pc.arguments)
                                .unwrap_or(Value::Object(Default::default()));
                            let _ = tx
                                .send(StreamEvent::ToolCallComplete(ToolCall {
                                    id: pc.id,
                                    name: pc.name,
                                    arguments: args,
                                }))
                                .await;
                        }
                        crate::introspect::record_call();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        let _ = tx.send(StreamEvent::Done).await;
                        return;
                    }

                    let parsed: StreamChunk = match serde_json::from_str(data) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    if let Some(usage) = parsed.usage {
                        let _ = tx
                            .send(StreamEvent::Usage(TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                            }))
                            .await;
                    }

                    if let Some(choices) = parsed.choices {
                        for choice in choices {
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    let _ = tx.send(StreamEvent::TextDelta(content)).await;
                                }
                            }

                            if let Some(tc_deltas) = choice.delta.tool_calls {
                                for tcd in tc_deltas {
                                    while partial_calls.len() <= tcd.index {
                                        partial_calls.push(PartialToolCall::default());
                                    }
                                    let pc = &mut partial_calls[tcd.index];
                                    pc.index = tcd.index;

                                    if let Some(id) = tcd.id {
                                        pc.id = id;
                                    }
                                    if let Some(func) = tcd.function {
                                        if let Some(name) = func.name {
                                            pc.name = name;
                                        }
                                        if let Some(args) = func.arguments {
                                            pc.arguments.push_str(&args);
                                        }
                                    }
                                }
                            }

                            if choice.finish_reason.as_deref() == Some("tool_calls") {
                                for pc in partial_calls.drain(..) {
                                    let args = serde_json::from_str(&pc.arguments)
                                        .unwrap_or(Value::Object(Default::default()));
                                    let _ = tx
                                        .send(StreamEvent::ToolCallComplete(ToolCall {
                                            id: pc.id,
                                            name: pc.name,
                                            arguments: args,
                                        }))
                                        .await;
                                }
                            }
                        }
                    }
                }
            }

            for pc in partial_calls.drain(..) {
                let args = serde_json::from_str(&pc.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                let _ = tx
                    .send(StreamEvent::ToolCallComplete(ToolCall {
                        id: pc.id,
                        name: pc.name,
                        arguments: args,
                    }))
                    .await;
            }
            crate::introspect::record_call();
            crate::introspect::record_llm_latency(stream_start.elapsed().as_millis() as u64);
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }

    async fn chat_with_tools_stream_responses(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = "OpenAI (Ouath)",
            model = %model,
            messages = messages.len(),
            has_tools,
            stream = true,
            "LLM request (responses streaming)"
        );

        let (instructions, input) = build_responses_input(messages);
        let api_tools = tool_schemas_to_api(tools);

        let mut req = serde_json::json!({
            "model": model,
            "input": input,
            "stream": true,
            "store": false,
        });
        if let Some(instr) = instructions {
            req["instructions"] = Value::String(instr);
        }
        if let Some(api_tools) = api_tools {
            req["tools"] = serde_json::to_value(api_tools).unwrap_or(Value::Null);
            req["tool_choice"] = Value::String("auto".into());
            req["parallel_tool_calls"] = Value::Bool(true);
        }
        if !self.config.url.contains("chatgpt.com") && !self.config.url.contains("backend-api") {
            if let Some(temp) = serde_json::Number::from_f64(self.config.temperature as f64) {
                req["temperature"] = Value::Number(temp);
            }
            req["max_output_tokens"] = Value::Number(self.config.max_tokens.into());
        }

        if self.config.reasoning_effort.is_some() || self.config.reasoning_summary.is_some() {
            let mut reasoning = serde_json::Map::new();
            if let Some(ref effort) = self.config.reasoning_effort {
                reasoning.insert("effort".into(), Value::String(effort.clone()));
            }
            if let Some(ref summary) = self.config.reasoning_summary {
                reasoning.insert("summary".into(), Value::String(summary.clone()));
            }
            req["reasoning"] = Value::Object(reasoning);
        }
        if !self.config.include.is_empty() {
            req["include"] = Value::Array(
                self.config
                    .include
                    .iter()
                    .map(|v| Value::String(v.clone()))
                    .collect(),
            );
        }

        let (resp, _start) = self
            .send_with_auth_retry(
                OuathApiStyle::Responses,
                &req,
                Some(("OpenAI-Beta", "responses=experimental")),
            )
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "OpenAI (Ouath)", %status, "LLM stream error");
            return Err(AthenaError::Llm(format!(
                "Ouath returned {}: {}",
                status, body
            )));
        }

        let (tx, rx) = mpsc::channel(64);
        let stream_start = Instant::now();

        tokio::spawn(async move {
            use tokio_stream::StreamExt;

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut partial_calls: HashMap<String, OuathPartialToolCall> = HashMap::new();
            let mut emitted_calls: HashSet<String> = HashSet::new();
            let mut final_response: Option<Value> = None;
            let mut text_emitted = false;
            let mut usage_emitted = false;

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(provider = "OpenAI (Ouath)", error = %e, "Stream read error");
                        crate::introspect::record_error();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline_pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=newline_pos).collect();
                    let line = line.trim();

                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    if data == "[DONE]" {
                        emit_responses_final(
                            &tx,
                            &mut partial_calls,
                            &mut emitted_calls,
                            &mut final_response,
                            &mut text_emitted,
                            &mut usage_emitted,
                        )
                        .await;
                        crate::introspect::record_call();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        let _ = tx.send(StreamEvent::Done).await;
                        return;
                    }

                    let parsed: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(event_type) = parsed.get("type").and_then(|v| v.as_str()) {
                        match event_type {
                            "response.output_text.delta" => {
                                if let Some(delta) = parsed.get("delta").and_then(|v| v.as_str()) {
                                    if !delta.is_empty() {
                                        text_emitted = true;
                                        let _ = tx.send(StreamEvent::TextDelta(delta.into())).await;
                                    }
                                }
                            }
                            "response.output_item.added" => {
                                if let Some(item) = parsed.get("item") {
                                    apply_tool_item(&mut partial_calls, item);
                                }
                            }
                            "response.output_item.delta" => {
                                let item_id = parsed
                                    .get("item_id")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                if let Some(delta) = parsed.get("delta") {
                                    apply_tool_delta(&mut partial_calls, item_id, delta);
                                }
                            }
                            "response.completed" => {
                                if let Some(response) = parsed.get("response") {
                                    final_response = Some(response.clone());
                                }
                            }
                            _ => {}
                        }
                    } else if let Some(response) = parsed.get("response") {
                        final_response = Some(response.clone());
                    }
                }
            }

            emit_responses_final(
                &tx,
                &mut partial_calls,
                &mut emitted_calls,
                &mut final_response,
                &mut text_emitted,
                &mut usage_emitted,
            )
            .await;
            crate::introspect::record_call();
            crate::introspect::record_llm_latency(stream_start.elapsed().as_millis() as u64);
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }
}

struct OuathPartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OuathPartialToolCall {
    fn new(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            arguments: String::new(),
        }
    }
}

async fn send_ouath_request<T: Serialize + ?Sized>(
    client: &Client,
    endpoint: &str,
    body: &T,
    tokens: &OuathTokens,
    require_account_id: bool,
    extra_header: Option<(&str, &str)>,
) -> Result<reqwest::Response> {
    let mut builder = client
        .post(endpoint)
        .bearer_auth(&tokens.access_token)
        .json(body);

    if let Some((key, value)) = extra_header {
        builder = builder.header(key, value);
    }
    if let Some(account_id) = &tokens.chatgpt_account_id {
        builder = builder.header("chatgpt-account-id", account_id);
    } else if require_account_id {
        return Err(AthenaError::Config(
            "Ouath tokens missing chatgpt_account_id. Re-authenticate.".into(),
        ));
    }

    Ok(builder.send().await?)
}

async fn collect_stream_response(
    mut rx: mpsc::Receiver<StreamEvent>,
) -> Result<(ChatResponse, Option<TokenUsage>)> {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<TokenUsage> = None;

    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::ToolCallComplete(tc) => tool_calls.push(tc),
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done => break,
        }
    }

    if !tool_calls.is_empty() {
        return Ok((
            ChatResponse::ToolCalls {
                tool_calls,
                text: if text.is_empty() { None } else { Some(text) },
            },
            usage,
        ));
    }

    Ok((ChatResponse::Text(text), usage))
}

fn apply_tool_item(partials: &mut HashMap<String, OuathPartialToolCall>, item: &Value) {
    if !is_tool_call_item(item) {
        return;
    }
    let id = extract_tool_call_id(item)
        .unwrap_or_else(|| format!("ouath-call-{}", uuid::Uuid::new_v4()));
    let entry = partials
        .entry(id.clone())
        .or_insert_with(|| OuathPartialToolCall::new(id));
    if entry.name.is_empty() {
        if let Some(name) = extract_tool_call_name(item) {
            entry.name = name;
        }
    }
    if let Some(args) = extract_tool_call_args(item) {
        entry.arguments.push_str(&args);
    }
}

fn apply_tool_delta(
    partials: &mut HashMap<String, OuathPartialToolCall>,
    item_id: Option<String>,
    delta: &Value,
) {
    let id = item_id
        .or_else(|| extract_tool_call_id(delta))
        .unwrap_or_else(|| format!("ouath-call-{}", uuid::Uuid::new_v4()));
    let entry = partials
        .entry(id.clone())
        .or_insert_with(|| OuathPartialToolCall::new(id));
    if entry.name.is_empty() {
        if let Some(name) = extract_tool_call_name(delta) {
            entry.name = name;
        }
    }
    if let Some(args) = extract_tool_call_args(delta) {
        entry.arguments.push_str(&args);
    }
}

async fn emit_responses_final(
    tx: &mpsc::Sender<StreamEvent>,
    partials: &mut HashMap<String, OuathPartialToolCall>,
    emitted: &mut HashSet<String>,
    final_response: &mut Option<Value>,
    text_emitted: &mut bool,
    usage_emitted: &mut bool,
) {
    if let Some(response) = final_response.take() {
        if let Ok((resp, usage)) = parse_responses_body(&response) {
            if let Some(usage) = usage {
                if !*usage_emitted {
                    *usage_emitted = true;
                    let _ = tx.send(StreamEvent::Usage(usage)).await;
                }
            }
            match resp {
                ChatResponse::Text(text) => {
                    if !text.is_empty() && !*text_emitted {
                        *text_emitted = true;
                        let _ = tx.send(StreamEvent::TextDelta(text)).await;
                    }
                }
                ChatResponse::ToolCalls { tool_calls, text } => {
                    if let Some(text) = text {
                        if !text.is_empty() && !*text_emitted {
                            *text_emitted = true;
                            let _ = tx.send(StreamEvent::TextDelta(text)).await;
                        }
                    }
                    for tc in tool_calls {
                        if emitted.insert(tc.id.clone()) {
                            let _ = tx.send(StreamEvent::ToolCallComplete(tc)).await;
                        }
                    }
                }
            }
        }
    }

    for (_, pc) in partials.drain() {
        if emitted.contains(&pc.id) {
            continue;
        }
        let args = serde_json::from_str(&pc.arguments).unwrap_or(Value::Object(Default::default()));
        let _ = tx
            .send(StreamEvent::ToolCallComplete(ToolCall {
                id: pc.id,
                name: pc.name,
                arguments: args,
            }))
            .await;
    }
}

fn is_tool_call_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(|v| v.as_str()),
        Some("tool_call") | Some("function_call")
    )
}

fn extract_tool_call_id(item: &Value) -> Option<String> {
    item.get("id")
        .or_else(|| item.get("tool_call_id"))
        .or_else(|| item.get("call_id"))
        .or_else(|| item.get("item_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_tool_call_name(item: &Value) -> Option<String> {
    item.get("name")
        .and_then(|v| v.as_str())
        .or_else(|| item.pointer("/function/name").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn extract_tool_call_args(item: &Value) -> Option<String> {
    if let Some(s) = item.get("arguments").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = item.pointer("/function/arguments").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    None
}

fn parse_responses_body(body: &Value) -> Result<(ChatResponse, Option<TokenUsage>)> {
    let usage = body.get("usage").and_then(|u| {
        let prompt_tokens = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let completion_tokens = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Some(TokenUsage {
            prompt_tokens,
            completion_tokens,
        })
    });

    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(output) = body.get("output").and_then(|o| o.as_array()) {
        for item in output {
            if let Some(calls) = item.get("tool_calls").and_then(|v| v.as_array()) {
                for call in calls {
                    if let Some(tc) = parse_tool_call(call) {
                        tool_calls.push(tc);
                    }
                }
            }

            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match item_type {
                "message" => {
                    if let Some(contents) = item.get("content").and_then(|v| v.as_array()) {
                        for part in contents {
                            let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if part_type == "output_text" || part_type == "text" {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                }
                "output_text" => {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                "tool_call" | "function_call" => {
                    if let Some(tc) = parse_tool_call(item) {
                        tool_calls.push(tc);
                    }
                }
                _ => {}
            }
        }
    }

    if !tool_calls.is_empty() {
        return Ok((
            ChatResponse::ToolCalls {
                tool_calls,
                text: if text.is_empty() { None } else { Some(text) },
            },
            usage,
        ));
    }

    Ok((ChatResponse::Text(text), usage))
}

fn parse_tool_call(item: &Value) -> Option<ToolCall> {
    let id = item
        .get("id")
        .or_else(|| item.get("tool_call_id"))
        .or_else(|| item.get("call_id"))
        .or_else(|| item.get("item_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("ouath-call-{}", uuid::Uuid::new_v4()));

    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| item.pointer("/function/name").and_then(|v| v.as_str()))
        .unwrap_or("unknown")
        .to_string();

    let args_val = item
        .get("arguments")
        .or_else(|| item.pointer("/function/arguments"))
        .cloned()
        .unwrap_or(Value::Null);

    let arguments = match args_val {
        Value::String(s) => try_parse_json(&s).unwrap_or(Value::String(s)),
        Value::Object(_) | Value::Array(_) => args_val,
        _ => Value::Null,
    };

    Some(ToolCall {
        id,
        name,
        arguments,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: ChatRole,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ollama-specific request/response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    options: OllamaChatOptions,
}

#[derive(Serialize)]
struct OllamaChatOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatResponseMessage,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Deserialize)]
struct OllamaChatResponseMessage {
    content: String,
}

pub struct OllamaClient {
    client: Client,
    config: OllamaConfig,
}

impl OllamaClient {
    pub fn new(config: OllamaConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaClient {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        tracing::info!(
            provider = "Ollama",
            model = %self.config.model,
            messages = messages.len(),
            "LLM request"
        );

        let req = OllamaChatRequest {
            model: self.config.model.clone(),
            messages: messages.to_vec(),
            stream: false,
            options: OllamaChatOptions {
                temperature: self.config.temperature,
                num_predict: self.config.max_tokens,
            },
        };

        let start = Instant::now();
        let resp = self
            .client
            .post(format!("{}/api/chat", self.config.url))
            .json(&req)
            .send()
            .await?;
        let latency = start.elapsed();
        crate::introspect::record_llm_latency(latency.as_millis() as u64);

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "Ollama", %status, "LLM error");
            crate::introspect::record_error();
            return Err(AthenaError::Llm(format!(
                "Ollama returned {}: {}",
                status, body
            )));
        }

        crate::introspect::record_call();
        let chat_resp: OllamaChatResponse = resp.json().await?;
        tracing::info!(
            provider = "Ollama",
            latency_ms = latency.as_millis() as u64,
            prompt_tokens = chat_resp.prompt_eval_count,
            completion_tokens = chat_resp.eval_count,
            response_len = chat_resp.message.content.len(),
            "LLM response"
        );
        Ok(chat_resp.message.content)
    }

    async fn health_check(&self) -> Result<()> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.config.url))
            .send()
            .await
            .map_err(|e| {
                AthenaError::Llm(format!("Cannot reach Ollama at {}: {}", self.config.url, e))
            })?;

        let body: Value = resp.json().await?;
        let models = body["models"]
            .as_array()
            .ok_or_else(|| AthenaError::Llm("Unexpected Ollama response".into()))?;

        let available = models.iter().any(|m| {
            m["name"]
                .as_str()
                .map(|n| n.starts_with(&self.config.model))
                .unwrap_or(false)
        });

        if !available {
            let names: Vec<&str> = models.iter().filter_map(|m| m["name"].as_str()).collect();
            return Err(AthenaError::Llm(format!(
                "Model '{}' not found. Available: {:?}",
                self.config.model, names
            )));
        }

        Ok(())
    }

    fn provider_name(&self) -> &str {
        "Ollama"
    }

    fn current_model(&self) -> String {
        self.config.model.clone()
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible client (OpenRouter, Opencode Zen, etc.)
// ---------------------------------------------------------------------------

pub struct OpenAiCompatibleConfig {
    pub url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub context_window: u64,
}

pub struct OpenAiCompatibleClient {
    client: Client,
    config: OpenAiCompatibleConfig,
    name: String,
    model_override: std::sync::RwLock<Option<String>>,
}

#[derive(Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
    max_tokens: u32,
}

/// Request with optional native tools support.
#[derive(Serialize)]
struct OpenAiChatRequestWithTools {
    model: String,
    messages: Vec<Value>,
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    model: Option<String>,
}

/// Full response that includes optional tool_calls.
#[derive(Deserialize)]
struct OpenAiChatResponseFull {
    choices: Vec<OpenAiChoiceFull>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiChoiceFull {
    message: OpenAiMessageFull,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[derive(Deserialize)]
struct OpenAiMessageFull {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl OpenAiCompatibleClient {
    pub fn new(config: OpenAiCompatibleConfig, name: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            config,
            name: name.into(),
            model_override: std::sync::RwLock::new(None),
        }
    }

    /// Return the model to use: override if set, otherwise config default.
    fn effective_model(&self) -> String {
        self.model_override
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.config.model.clone())
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleClient {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        let model = self.effective_model();
        tracing::info!(
            provider = %self.name,
            model = %model,
            messages = messages.len(),
            "LLM request"
        );

        let req = OpenAiChatRequest {
            model,
            messages: messages.to_vec(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
        };

        let start = Instant::now();
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.config.url))
            .bearer_auth(&self.config.api_key)
            .json(&req)
            .send()
            .await?;
        let latency = start.elapsed();
        crate::introspect::record_llm_latency(latency.as_millis() as u64);

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = %self.name, %status, "LLM error");
            crate::introspect::record_error();
            return Err(AthenaError::Llm(format!(
                "{} returned {}: {}",
                self.name, status, body
            )));
        }

        crate::introspect::record_call();
        let chat_resp: OpenAiChatResponse = resp.json().await?;
        let (prompt_tok, completion_tok) = chat_resp
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let resolved_model = chat_resp.model.as_deref().unwrap_or(&self.config.model);
        tracing::info!(
            provider = %self.name,
            model = resolved_model,
            latency_ms = latency.as_millis() as u64,
            prompt_tokens = prompt_tok,
            completion_tokens = completion_tok,
            "LLM response"
        );

        let content = chat_resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AthenaError::Llm(format!("{} returned empty choices", self.name)))?;
        Ok(content)
    }

    async fn health_check(&self) -> Result<()> {
        let resp = self
            .client
            .get(format!("{}/models", self.config.url))
            .bearer_auth(&self.config.api_key)
            .send()
            .await
            .map_err(|e| {
                AthenaError::Llm(format!(
                    "Cannot reach {} at {}: {}",
                    self.name, self.config.url, e
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AthenaError::Llm(format!(
                "{} health check failed ({}): {}",
                self.name, status, body
            )));
        }

        Ok(())
    }

    fn provider_name(&self) -> &str {
        &self.name
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn context_window(&self) -> u64 {
        self.config.context_window
    }

    fn current_model(&self) -> String {
        self.effective_model()
    }

    fn set_model_override(&self, model: Option<String>) {
        *self
            .model_override
            .write()
            .unwrap_or_else(|e| e.into_inner()) = model;
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(format!("{}/models", self.config.url))
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AthenaError::Llm(format!(
                "{} /models returned {}: {}",
                self.name, status, body
            )));
        }

        let body: Value = resp.json().await?;
        let mut models: Vec<String> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        models.sort();
        Ok(models)
    }

    #[cfg(feature = "telegram")]
    async fn credits(&self) -> Result<Option<(f64, f64)>> {
        // Only supported for OpenRouter (URL contains openrouter.ai)
        if !self.config.url.contains("openrouter.ai") {
            return Ok(None);
        }

        let resp = self
            .client
            .get(format!("{}/credits", self.config.url))
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            tracing::debug!(
                provider = %self.name,
                status = %resp.status(),
                "Credits endpoint unavailable"
            );
            return Ok(None);
        }

        let body: Value = resp.json().await?;
        let total = body["data"]["total_credits"].as_f64().unwrap_or(0.0);
        let used = body["data"]["total_usage"].as_f64().unwrap_or(0.0);
        Ok(Some((total, used)))
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<(ChatResponse, Option<TokenUsage>)> {
        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = %self.name,
            model = %model,
            messages = messages.len(),
            has_tools,
            "LLM request (with tools)"
        );

        // Convert ChatMessage to wire-format JSON values
        let wire_messages: Vec<Value> = messages
            .iter()
            .filter_map(|m| chat_message_to_wire(m))
            .collect();

        // Convert ToolSchema to API format
        let api_tools = tool_schemas_to_api(tools);

        let req = OpenAiChatRequestWithTools {
            model,
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            tools: api_tools,
            stream: None,
            stream_options: None,
        };

        let start = Instant::now();
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.config.url))
            .bearer_auth(&self.config.api_key)
            .json(&req)
            .send()
            .await?;
        let latency = start.elapsed();
        crate::introspect::record_llm_latency(latency.as_millis() as u64);

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            // Graceful fallback: if the API rejects the tools parameter,
            // retry without tools using the simple chat() path
            if status.as_u16() == 400 && (body.contains("tools") || body.contains("function")) {
                tracing::warn!(
                    provider = %self.name,
                    "API rejected tools parameter, falling back to chat()"
                );
                let simple: Vec<Message> = messages.iter().filter_map(|m| m.to_simple()).collect();
                let text = self.chat(&simple).await?;
                return Ok((ChatResponse::Text(text), None));
            }

            tracing::error!(provider = %self.name, %status, "LLM error (tools)");
            crate::introspect::record_error();
            return Err(AthenaError::Llm(format!(
                "{} returned {}: {}",
                self.name, status, body
            )));
        }

        crate::introspect::record_call();
        let chat_resp: OpenAiChatResponseFull = resp.json().await?;
        let (prompt_tok, completion_tok) = chat_resp
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let resolved_model = chat_resp.model.as_deref().unwrap_or(&self.config.model);

        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AthenaError::Llm(format!("{} returned empty choices", self.name)))?;

        let n_tool_calls = choice
            .message
            .tool_calls
            .as_ref()
            .map(|tc| tc.len())
            .unwrap_or(0);

        tracing::info!(
            provider = %self.name,
            model = resolved_model,
            latency_ms = latency.as_millis() as u64,
            prompt_tokens = prompt_tok,
            completion_tokens = completion_tok,
            tool_calls = n_tool_calls,
            "LLM response (with tools)"
        );

        let usage = Some(TokenUsage {
            prompt_tokens: prompt_tok,
            completion_tokens: completion_tok,
        });

        // Parse tool_calls if present
        if let Some(api_calls) = choice.message.tool_calls {
            if !api_calls.is_empty() {
                let tool_calls: Vec<ToolCall> = api_calls
                    .into_iter()
                    .filter_map(|tc| {
                        let args: Value = serde_json::from_str(&tc.function.arguments).ok()?;
                        Some(ToolCall {
                            id: tc.id,
                            name: tc.function.name,
                            arguments: args,
                        })
                    })
                    .collect();
                return Ok((
                    ChatResponse::ToolCalls {
                        tool_calls,
                        text: choice.message.content,
                    },
                    usage,
                ));
            }
        }

        Ok((
            ChatResponse::Text(choice.message.content.unwrap_or_default()),
            usage,
        ))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_tools_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        let has_tools = !tools.is_empty();
        let model = self.effective_model();
        tracing::info!(
            provider = %self.name,
            model = %model,
            messages = messages.len(),
            has_tools,
            stream = true,
            "LLM request (streaming)"
        );

        let wire_messages: Vec<Value> = messages
            .iter()
            .filter_map(|m| chat_message_to_wire(m))
            .collect();

        let api_tools = tool_schemas_to_api(tools);

        let req = OpenAiChatRequestWithTools {
            model,
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            tools: api_tools,
            stream: Some(true),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.config.url))
            .bearer_auth(&self.config.api_key)
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = %self.name, %status, "LLM stream error");
            return Err(AthenaError::Llm(format!(
                "{} returned {}: {}",
                self.name, status, body
            )));
        }

        let (tx, rx) = mpsc::channel(64);
        let provider_name = self.name.clone();
        let stream_start = Instant::now();

        // Spawn a task to read SSE lines and emit StreamEvents
        tokio::spawn(async move {
            use tokio_stream::StreamExt;

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut partial_calls: Vec<PartialToolCall> = Vec::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(provider = %provider_name, error = %e, "Stream read error");
                        crate::introspect::record_error();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete SSE lines
                while let Some(newline_pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=newline_pos).collect();
                    let line = line.trim();

                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }

                    if !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];

                    if data == "[DONE]" {
                        // Emit any remaining partial tool calls
                        for pc in partial_calls.drain(..) {
                            let args = serde_json::from_str(&pc.arguments)
                                .unwrap_or(Value::Object(Default::default()));
                            let _ = tx
                                .send(StreamEvent::ToolCallComplete(ToolCall {
                                    id: pc.id,
                                    name: pc.name,
                                    arguments: args,
                                }))
                                .await;
                        }
                        crate::introspect::record_call();
                        crate::introspect::record_llm_latency(
                            stream_start.elapsed().as_millis() as u64
                        );
                        let _ = tx.send(StreamEvent::Done).await;
                        return;
                    }

                    let parsed: StreamChunk = match serde_json::from_str(data) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    // Handle usage (usually in the final chunk)
                    if let Some(usage) = parsed.usage {
                        let _ = tx
                            .send(StreamEvent::Usage(TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                            }))
                            .await;
                    }

                    if let Some(choices) = parsed.choices {
                        for choice in choices {
                            // Text content delta
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    let _ = tx.send(StreamEvent::TextDelta(content)).await;
                                }
                            }

                            // Tool call deltas
                            if let Some(tc_deltas) = choice.delta.tool_calls {
                                for tcd in tc_deltas {
                                    // Find or create partial call at this index
                                    while partial_calls.len() <= tcd.index {
                                        partial_calls.push(PartialToolCall::default());
                                    }
                                    let pc = &mut partial_calls[tcd.index];
                                    pc.index = tcd.index;

                                    if let Some(id) = tcd.id {
                                        pc.id = id;
                                    }
                                    if let Some(func) = tcd.function {
                                        if let Some(name) = func.name {
                                            pc.name = name;
                                        }
                                        if let Some(args) = func.arguments {
                                            pc.arguments.push_str(&args);
                                        }
                                    }
                                }
                            }

                            // If finish_reason is "tool_calls", emit all partial calls
                            if choice.finish_reason.as_deref() == Some("tool_calls") {
                                for pc in partial_calls.drain(..) {
                                    let args = serde_json::from_str(&pc.arguments)
                                        .unwrap_or(Value::Object(Default::default()));
                                    let _ = tx
                                        .send(StreamEvent::ToolCallComplete(ToolCall {
                                            id: pc.id,
                                            name: pc.name,
                                            arguments: args,
                                        }))
                                        .await;
                                }
                            }
                        }
                    }
                }
            }

            // Stream ended without [DONE] — emit remaining partial calls + Done
            for pc in partial_calls.drain(..) {
                let args = serde_json::from_str(&pc.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                let _ = tx
                    .send(StreamEvent::ToolCallComplete(ToolCall {
                        id: pc.id,
                        name: pc.name,
                        arguments: args,
                    }))
                    .await;
            }
            crate::introspect::record_call();
            crate::introspect::record_llm_latency(stream_start.elapsed().as_millis() as u64);
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(rx)
    }
}

/// Convert a `ChatMessage` to the OpenAI wire-format JSON value.
fn chat_message_to_wire(msg: &ChatMessage) -> Option<Value> {
    match msg {
        ChatMessage::System(s) => Some(serde_json::json!({
            "role": "system",
            "content": s,
        })),
        ChatMessage::User(s) => Some(serde_json::json!({
            "role": "user",
            "content": s,
        })),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut obj = serde_json::json!({ "role": "assistant" });
            if let Some(c) = content {
                obj["content"] = Value::String(c.clone());
            }
            if let Some(tcs) = tool_calls {
                let wire_calls: Vec<Value> = tcs
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                obj["tool_calls"] = Value::Array(wire_calls);
            }
            Some(obj)
        }
        ChatMessage::Tool {
            tool_call_id,
            content,
        } => Some(serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        })),
    }
}

// ---------------------------------------------------------------------------
// JSON extraction helpers
// ---------------------------------------------------------------------------

/// Static regex for trailing comma cleanup (compiled once)
static TRAILING_COMMA_RE: std::sync::LazyLock<Option<regex::Regex>> =
    std::sync::LazyLock::new(|| match regex::Regex::new(r",\s*([}\]])") {
        Ok(re) => Some(re),
        Err(e) => {
            tracing::error!("Invalid trailing comma regex: {}", e);
            None
        }
    });

/// Sanitize common LLM JSON errors:
/// - \' → ' (invalid JSON escape, common in shell-influenced output)
/// - Trailing commas before } or ]
fn sanitize_json(text: &str) -> String {
    let mut out = text.to_string();
    // Fix invalid \' escape (single quotes don't need escaping in JSON)
    out = out.replace("\\'", "'");
    // Fix trailing commas: , } or , ]
    if let Some(re) = TRAILING_COMMA_RE.as_ref() {
        out = re.replace_all(&out, "$1").to_string();
    }
    out
}

/// Try parsing JSON with sanitization fallback
fn try_parse_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    // Retry with sanitized text
    let sanitized = sanitize_json(text);
    serde_json::from_str::<Value>(&sanitized).ok()
}

/// Extract JSON from LLM text output.
/// Handles: raw JSON, ```json blocks, JSON embedded in prose,
/// and common LLM JSON errors (invalid escapes, trailing commas).
pub fn extract_json(text: &str) -> Option<Value> {
    extract_json_raw(text)
        .or_else(|| extract_json_fenced(text))
        .or_else(|| extract_json_embedded(text))
}

fn extract_json_raw(text: &str) -> Option<Value> {
    try_parse_json(text.trim())
}

fn extract_json_fenced(text: &str) -> Option<Value> {
    let mut idx = 0usize;
    while let Some(start) = text[idx..].find("```") {
        let fence_start = idx + start;
        let after = &text[fence_start + 3..];
        let Some(end) = after.find("```") else {
            break;
        };
        let block = after[..end].trim();
        let block = if let Some(nl) = block.find('\n') {
            let first_line = &block[..nl];
            if !first_line.contains('{') && !first_line.contains('[') {
                &block[nl + 1..]
            } else {
                block
            }
        } else {
            block
        };
        if let Some(v) = try_parse_json(block.trim()) {
            return Some(v);
        }
        idx = fence_start + 3 + end + 3;
    }
    None
}

fn extract_json_embedded(text: &str) -> Option<Value> {
    // Try finding first { ... } in the text
    let mut depth = 0i32;
    let mut start = None;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let candidate = &text[s..=i];
                        if let Some(v) = try_parse_json(candidate) {
                            return Some(v);
                        }
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_raw() {
        let text = r#"{"tool": "shell", "params": {"command": "ls"}}"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
    }

    #[test]
    fn test_extract_json_markdown() {
        let text = "Here's the command:\n```json\n{\"tool\": \"shell\", \"params\": {\"command\": \"ls\"}}\n```";
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
    }

    #[test]
    fn test_extract_json_multiple_fenced_blocks_prefers_first_valid() {
        let text = "```json\n{\"tool\": \"shell\", \"params\": {\"command\": \"ls\"}}\n```\n```json\n{\"tool\": \"shell\", \"params\": {\"command\": \"pwd\"}}\n```";
        let v = extract_json(text).unwrap();
        assert_eq!(v["params"]["command"], "ls");
    }

    #[test]
    fn test_extract_json_embedded() {
        let text =
            "I'll run this: {\"tool\": \"shell\", \"params\": {\"command\": \"ls\"}} and check.";
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
    }

    #[test]
    fn test_extract_json_none() {
        assert!(extract_json("just plain text").is_none());
    }

    #[test]
    fn test_extract_json_invalid_escape() {
        // LLMs often produce \' which is invalid JSON
        let text = r#"{"tool": "shell", "params": {"command": "grep -rH \'TODO\' src/"}}"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
        assert_eq!(v["params"]["command"], "grep -rH 'TODO' src/");
    }

    #[test]
    fn test_extract_json_trailing_comma() {
        let text = r#"{"tool": "shell", "params": {"command": "ls",}}"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
    }

    // ── ChatMessage::to_simple ────────────────────────────────────────

    #[test]
    fn test_chat_message_to_simple_system() {
        let msg = ChatMessage::System("hello".into());
        let simple = msg.to_simple().unwrap();
        assert!(matches!(simple.role, ChatRole::System));
        assert_eq!(simple.content, "hello");
    }

    #[test]
    fn test_chat_message_to_simple_user() {
        let msg = ChatMessage::User("input".into());
        let simple = msg.to_simple().unwrap();
        assert!(matches!(simple.role, ChatRole::User));
        assert_eq!(simple.content, "input");
    }

    #[test]
    fn test_chat_message_to_simple_assistant_with_content() {
        let msg = ChatMessage::Assistant {
            content: Some("response".into()),
            tool_calls: None,
        };
        let simple = msg.to_simple().unwrap();
        assert!(matches!(simple.role, ChatRole::Assistant));
        assert_eq!(simple.content, "response");
    }

    #[test]
    fn test_chat_message_to_simple_assistant_no_content() {
        let msg = ChatMessage::Assistant {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }]),
        };
        assert!(msg.to_simple().is_none());
    }

    #[test]
    fn test_chat_message_to_simple_tool() {
        let msg = ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "result".into(),
        };
        let simple = msg.to_simple().unwrap();
        assert!(matches!(simple.role, ChatRole::User));
        assert_eq!(simple.content, "result");
    }

    // ── OpenAiChatResponseFull deserialization ────────────────────────

    #[test]
    fn test_response_full_with_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\": \"ls\"}"
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;
        let resp: OpenAiChatResponseFull = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].function.name, "shell");
        assert!(resp.choices[0].message.content.is_none());
    }

    #[test]
    fn test_response_full_text_only() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "Hello world"
                }
            }]
        }"#;
        let resp: OpenAiChatResponseFull = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert!(resp.choices[0].message.tool_calls.is_none());
    }

    // ── ToolSchema serialization ─────────────────────────────────────

    #[test]
    fn test_tool_schema_serialization() {
        let schema = ToolSchema {
            name: "shell".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        };
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["name"], "shell");
        assert_eq!(json["parameters"]["required"][0], "command");
    }

    // ── TokenBudget ────────────────────────────────────────────────────

    #[test]
    fn test_token_budget_new() {
        let budget = TokenBudget::new(128_000);
        assert_eq!(budget.context_window, 128_000);
        assert_eq!(budget.reserved_for_completion, 32_000);
        assert_eq!(budget.call_count, 0);
        assert_eq!(budget.utilization(), 0.0);
    }

    #[test]
    fn test_token_budget_record_usage() {
        let mut budget = TokenBudget::new(100_000);
        budget.record_usage(&TokenUsage {
            prompt_tokens: 50_000,
            completion_tokens: 1_000,
        });
        assert_eq!(budget.call_count, 1);
        assert_eq!(budget.last_prompt_tokens, 50_000);
        assert_eq!(budget.total_completion_tokens, 1_000);
        assert!((budget.utilization() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_token_budget_needs_compression() {
        let mut budget = TokenBudget::new(100_000);
        budget.record_usage(&TokenUsage {
            prompt_tokens: 70_000,
            completion_tokens: 500,
        });
        assert!(!budget.needs_compression(0.80));

        budget.record_usage(&TokenUsage {
            prompt_tokens: 85_000,
            completion_tokens: 500,
        });
        assert!(budget.needs_compression(0.80));
    }

    #[test]
    fn test_token_budget_cumulative_completion() {
        let mut budget = TokenBudget::new(128_000);
        budget.record_usage(&TokenUsage {
            prompt_tokens: 10_000,
            completion_tokens: 500,
        });
        budget.record_usage(&TokenUsage {
            prompt_tokens: 20_000,
            completion_tokens: 600,
        });
        assert_eq!(budget.total_completion_tokens, 1_100);
        assert_eq!(budget.call_count, 2);
        // last_prompt_tokens should be the most recent
        assert_eq!(budget.last_prompt_tokens, 20_000);
    }

    // ── SSE stream chunk parsing ──────────────────────────────────────

    #[test]
    fn test_stream_chunk_text_delta() {
        let json = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk: StreamChunk = serde_json::from_str(json).unwrap();
        let choices = chunk.choices.unwrap();
        assert_eq!(choices[0].delta.content.as_deref(), Some("Hello"));
        assert!(choices[0].delta.tool_calls.is_none());
    }

    #[test]
    fn test_stream_chunk_tool_call_delta() {
        let json = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk: StreamChunk = serde_json::from_str(json).unwrap();
        let choices = chunk.choices.unwrap();
        let tc_deltas = choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(tc_deltas[0].index, 0);
        assert_eq!(tc_deltas[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            tc_deltas[0].function.as_ref().unwrap().name.as_deref(),
            Some("shell")
        );
    }

    #[test]
    fn test_stream_chunk_usage() {
        let json = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50}}"#;
        let chunk: StreamChunk = serde_json::from_str(json).unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
    }

    #[test]
    fn test_partial_tool_call_assembly() {
        let mut pc = PartialToolCall::default();
        pc.id = "call_1".into();
        pc.name = "shell".into();
        pc.arguments.push_str("{\"comm");
        pc.arguments.push_str("and\": \"ls\"}");

        let args: Value = serde_json::from_str(&pc.arguments).unwrap();
        assert_eq!(args["command"], "ls");
    }

    // ── compress_history ──────────────────────────────────────────────

    #[test]
    fn test_compress_history_preserves_head_and_tail() {
        use crate::strategy::react::compress_history;

        let mut history = vec![
            ChatMessage::System("system prompt".into()),
            ChatMessage::User("initial goal".into()),
        ];

        // Add 10 assistant/tool pairs (20 messages) = 22 total
        for i in 0..10 {
            history.push(ChatMessage::Assistant {
                content: Some(format!("thinking step {}", i)),
                tool_calls: Some(vec![ToolCall {
                    id: format!("call_{}", i),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": format!("cmd_{}", i)}),
                }]),
            });
            history.push(ChatMessage::Tool {
                tool_call_id: format!("call_{}", i),
                content: format!("result of cmd_{}", i),
            });
        }

        assert_eq!(history.len(), 22);
        compress_history(&mut history);

        // Should have: 2 (head) + 1 (summary) + 6 (tail) = 9
        assert_eq!(history.len(), 9);

        // First two preserved
        assert!(matches!(&history[0], ChatMessage::System(s) if s == "system prompt"));
        assert!(matches!(&history[1], ChatMessage::User(s) if s == "initial goal"));

        // Summary message
        assert!(matches!(&history[2], ChatMessage::User(s) if s.contains("[Compressed")));

        // Last 6 preserved (messages 16..22 from original)
        assert!(matches!(&history[3], ChatMessage::Assistant { .. }));
    }

    #[test]
    fn test_compress_history_noop_when_short() {
        use crate::strategy::react::compress_history;

        let mut history = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User("goal".into()),
            ChatMessage::Assistant {
                content: Some("ok".into()),
                tool_calls: None,
            },
        ];

        let original_len = history.len();
        compress_history(&mut history);
        assert_eq!(history.len(), original_len); // no change
    }
}
