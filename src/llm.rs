use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::OllamaConfig;
use crate::error::{AthenaError, Result};

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<String>;
    async fn health_check(&self) -> Result<()>;
    fn provider_name(&self) -> &str;
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

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
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
struct OpenAiMessage {
    content: String,
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
}

// ---------------------------------------------------------------------------
// JSON extraction helpers
// ---------------------------------------------------------------------------

/// Sanitize common LLM JSON errors:
/// - \' → ' (invalid JSON escape, common in shell-influenced output)
/// - Trailing commas before } or ]
fn sanitize_json(text: &str) -> String {
    let mut out = text.to_string();
    // Fix invalid \' escape (single quotes don't need escaping in JSON)
    out = out.replace("\\'", "'");
    // Fix trailing commas: , } or , ]
    let re = regex::Regex::new(r",\s*([}\]])").unwrap();
    out = re.replace_all(&out, "$1").to_string();
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
}
