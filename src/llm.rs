use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::OllamaConfig;
use crate::error::{AthenaError, Result};

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

    /// Chat with native tool definitions. Default: falls back to `chat()`.
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let simple: Vec<Message> = messages.iter().filter_map(|m| m.to_simple()).collect();
        let text = self.chat(&simple).await?;
        Ok(ChatResponse::Text(text))
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
        Self { role: ChatRole::System, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: ChatRole::User, content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: ChatRole::Assistant, content: content.into() }
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
        let resp = self.client
            .post(format!("{}/api/chat", self.config.url))
            .json(&req)
            .send()
            .await?;
        let latency = start.elapsed();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = "Ollama", %status, "LLM error");
            return Err(AthenaError::Llm(format!("Ollama returned {}: {}", status, body)));
        }

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
        let resp = self.client
            .get(format!("{}/api/tags", self.config.url))
            .send()
            .await
            .map_err(|e| AthenaError::Llm(format!("Cannot reach Ollama at {}: {}", self.config.url, e)))?;

        let body: Value = resp.json().await?;
        let models = body["models"].as_array()
            .ok_or_else(|| AthenaError::Llm("Unexpected Ollama response".into()))?;

        let available = models.iter().any(|m| {
            m["name"].as_str()
                .map(|n| n.starts_with(&self.config.model))
                .unwrap_or(false)
        });

        if !available {
            let names: Vec<&str> = models.iter()
                .filter_map(|m| m["name"].as_str())
                .collect();
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
}

pub struct OpenAiCompatibleClient {
    client: Client,
    config: OpenAiCompatibleConfig,
    name: String,
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
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleClient {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        tracing::info!(
            provider = %self.name,
            model = %self.config.model,
            messages = messages.len(),
            "LLM request"
        );

        let req = OpenAiChatRequest {
            model: self.config.model.clone(),
            messages: messages.to_vec(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
        };

        let start = Instant::now();
        let resp = self.client
            .post(format!("{}/chat/completions", self.config.url))
            .bearer_auth(&self.config.api_key)
            .json(&req)
            .send()
            .await?;
        let latency = start.elapsed();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(provider = %self.name, %status, "LLM error");
            return Err(AthenaError::Llm(format!("{} returned {}: {}", self.name, status, body)));
        }

        let chat_resp: OpenAiChatResponse = resp.json().await?;
        let (prompt_tok, completion_tok) = chat_resp.usage
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

        let content = chat_resp.choices.into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AthenaError::Llm(format!("{} returned empty choices", self.name)))?;
        Ok(content)
    }

    async fn health_check(&self) -> Result<()> {
        let resp = self.client
            .get(format!("{}/models", self.config.url))
            .bearer_auth(&self.config.api_key)
            .send()
            .await
            .map_err(|e| AthenaError::Llm(format!("Cannot reach {} at {}: {}", self.name, self.config.url, e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AthenaError::Llm(format!("{} health check failed ({}): {}", self.name, status, body)));
        }

        Ok(())
    }

    fn provider_name(&self) -> &str {
        &self.name
    }

    fn supports_tools(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let has_tools = !tools.is_empty();
        tracing::info!(
            provider = %self.name,
            model = %self.config.model,
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
            model: self.config.model.clone(),
            messages: wire_messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            tools: api_tools,
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            // Graceful fallback: if the API rejects the tools parameter,
            // retry without tools using the simple chat() path
            if status.as_u16() == 400
                && (body.contains("tools") || body.contains("function"))
            {
                tracing::warn!(
                    provider = %self.name,
                    "API rejected tools parameter, falling back to chat()"
                );
                let simple: Vec<Message> =
                    messages.iter().filter_map(|m| m.to_simple()).collect();
                let text = self.chat(&simple).await?;
                return Ok(ChatResponse::Text(text));
            }

            tracing::error!(provider = %self.name, %status, "LLM error (tools)");
            return Err(AthenaError::Llm(format!(
                "{} returned {}: {}",
                self.name, status, body
            )));
        }

        let chat_resp: OpenAiChatResponseFull = resp.json().await?;
        let (prompt_tok, completion_tok) = chat_resp
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        let resolved_model = chat_resp
            .model
            .as_deref()
            .unwrap_or(&self.config.model);

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

        // Parse tool_calls if present
        if let Some(api_calls) = choice.message.tool_calls {
            if !api_calls.is_empty() {
                let tool_calls: Vec<ToolCall> = api_calls
                    .into_iter()
                    .filter_map(|tc| {
                        let args: Value =
                            serde_json::from_str(&tc.function.arguments).ok()?;
                        Some(ToolCall {
                            id: tc.id,
                            name: tc.function.name,
                            arguments: args,
                        })
                    })
                    .collect();
                return Ok(ChatResponse::ToolCalls {
                    tool_calls,
                    text: choice.message.content,
                });
            }
        }

        Ok(ChatResponse::Text(
            choice.message.content.unwrap_or_default(),
        ))
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
        ChatMessage::Assistant { content, tool_calls } => {
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
        ChatMessage::Tool { tool_call_id, content } => Some(serde_json::json!({
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

/// Extract JSON from LLM text output.
/// Handles: raw JSON, ```json blocks, JSON embedded in prose,
/// and common LLM JSON errors (invalid escapes, trailing commas).
pub fn extract_json(text: &str) -> Option<Value> {
    // Try parsing the whole thing first
    if let Some(v) = try_parse_json(text.trim()) {
        return Some(v);
    }

    // Try extracting from ```json ... ``` blocks
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            if let Some(v) = try_parse_json(after[..end].trim()) {
                return Some(v);
            }
        }
    }

    // Try extracting from ``` ... ``` blocks (no json tag)
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            let block = after[..end].trim();
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
        }
    }

    // Try finding first { ... } in the text
    let mut depth = 0i32;
    let mut start = None;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 { start = Some(i); }
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
    fn test_extract_json_embedded() {
        let text = "I'll run this: {\"tool\": \"shell\", \"params\": {\"command\": \"ls\"}} and check.";
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
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello world"));
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
}
