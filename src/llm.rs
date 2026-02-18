use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::OllamaConfig;
use crate::error::{AthenaError, Result};

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
    pub reserved_for_completion: u64,
    pub last_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub call_count: u32,
}

impl TokenBudget {
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            reserved_for_completion: context_window / 4, // 25% reserved
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

    /// Query account credits (total, used). Returns None if not supported.
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
            .unwrap()
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
        *self.model_override.write().unwrap() = model;
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
            // Non-fatal: might be a non-management key
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
        let api_tools: Option<Vec<ApiToolDefinition>> = if has_tools {
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
        } else {
            None
        };

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

        let api_tools: Option<Vec<ApiToolDefinition>> = if has_tools {
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
        } else {
            None
        };

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
static TRAILING_COMMA_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r",\s*([}\]])").unwrap());

/// Sanitize common LLM JSON errors:
/// - \' → ' (invalid JSON escape, common in shell-influenced output)
/// - Trailing commas before } or ]
fn sanitize_json(text: &str) -> String {
    let mut out = text.to_string();
    // Fix invalid \' escape (single quotes don't need escaping in JSON)
    out = out.replace("\\'", "'");
    // Fix trailing commas: , } or , ]
    out = TRAILING_COMMA_RE.replace_all(&out, "$1").to_string();
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

fn extract_json_from_raw(text: &str) -> Option<Value> {
    try_parse_json(text.trim())
}

fn extract_json_from_fenced_blocks(text: &str) -> Option<Value> {
    extract_json_from_tagged_fenced_blocks(text)
        .or_else(|| extract_json_from_any_fenced_blocks(text))
}

fn extract_json_from_tagged_fenced_blocks(text: &str) -> Option<Value> {
    let mut offset = 0usize;
    while let Some(rel_start) = text[offset..].find("```json") {
        let start = offset + rel_start;
        let content_start = start + 7;
        let after = &text[content_start..];
        let Some(rel_end) = after.find("```") else {
            break;
        };
        let content_end = content_start + rel_end;
        if let Some(v) = try_parse_json(text[content_start..content_end].trim()) {
            return Some(v);
        }
        offset = content_end + 3;
    }
    None
}

fn extract_json_from_any_fenced_blocks(text: &str) -> Option<Value> {
    let mut offset = 0usize;
    while let Some(rel_start) = text[offset..].find("```") {
        let start = offset + rel_start;
        let content_start = start + 3;
        let after = &text[content_start..];
        let Some(rel_end) = after.find("```") else {
            break;
        };
        let content_end = content_start + rel_end;

        let block = text[content_start..content_end].trim();
        // Skip language tag if present (e.g., ```python)
        let block = if let Some(nl) = block.find('\n') {
            let first_line = &block[..nl];
            if !first_line.contains('{') {
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
        offset = content_end + 3;
    }
    None
}

fn extract_json_from_embedded_object(text: &str) -> Option<Value> {
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

/// Extract JSON from LLM text output.
/// Handles: raw JSON, ```json blocks, JSON embedded in prose,
/// and common LLM JSON errors (invalid escapes, trailing commas).
pub fn extract_json(text: &str) -> Option<Value> {
    extract_json_from_raw(text)
        .or_else(|| extract_json_from_fenced_blocks(text))
        .or_else(|| extract_json_from_embedded_object(text))
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

    #[test]
    fn test_extract_json_multiple_fenced_blocks_prefers_first_valid_json() {
        let text = r#"first:
```json
{invalid json}
```
second:
```json
{"tool":"shell","params":{"command":"ls"}}
```
third:
```json
{"tool":"shell","params":{"command":"pwd"}}
```"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["tool"], "shell");
        assert_eq!(v["params"]["command"], "ls");
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
