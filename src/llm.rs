use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::OllamaConfig;
use crate::error::{AthenaError, Result};

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

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    options: ChatOptions,
}

#[derive(Serialize)]
struct ChatOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
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

    /// Send a chat completion request
    pub async fn chat(&self, messages: &[Message]) -> Result<String> {
        let req = ChatRequest {
            model: self.config.model.clone(),
            messages: messages.to_vec(),
            stream: false,
            options: ChatOptions {
                temperature: self.config.temperature,
                num_predict: self.config.max_tokens,
            },
        };

        let resp = self.client
            .post(format!("{}/api/chat", self.config.url))
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AthenaError::Llm(format!("Ollama returned {}: {}", status, body)));
        }

        let chat_resp: ChatResponse = resp.json().await?;
        Ok(chat_resp.message.content)
    }

    /// Check if Ollama is reachable and the model is available
    pub async fn health_check(&self) -> Result<()> {
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
}

/// Extract JSON from LLM text output.
/// Handles: raw JSON, ```json blocks, JSON embedded in prose.
pub fn extract_json(text: &str) -> Option<Value> {
    // Try parsing the whole thing first
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }

    // Try extracting from ```json ... ``` blocks
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            if let Ok(v) = serde_json::from_str::<Value>(after[..end].trim()) {
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
            if let Ok(v) = serde_json::from_str::<Value>(block.trim()) {
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
                        if let Ok(v) = serde_json::from_str::<Value>(candidate) {
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
}
