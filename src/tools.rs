use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

use crate::config::GhostConfig;
use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};

const MAX_OUTPUT_LEN: usize = 2000;

#[derive(Debug)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn needs_confirmation(&self) -> bool;
    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult>;
}

/// Sensitive filenames that should never be read or written inside containers
const SENSITIVE_FILENAMES: &[&str] = &[
    "config.toml",
    ".env",
    ".env.local",
    "credentials.json",
    "secrets.toml",
];

/// Sensitive file extensions
const SENSITIVE_EXTENSIONS: &[&str] = &[".pem", ".key"];

/// Validate a path for safety: no traversal, must be under /workspace, no sensitive files
fn validate_path(path: &str) -> std::result::Result<(), &'static str> {
    // Reject path traversal
    if path.contains("..") {
        return Err("Path traversal (..) not allowed");
    }

    // Reject absolute paths outside /workspace
    if path.starts_with('/') && !path.starts_with("/workspace") {
        return Err("Absolute paths must be under /workspace");
    }

    // Check filename against sensitive names
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    for &sensitive in SENSITIVE_FILENAMES {
        if filename == sensitive {
            return Err("Access to sensitive file denied");
        }
    }

    for &ext in SENSITIVE_EXTENSIONS {
        if filename.ends_with(ext) {
            return Err("Access to sensitive file type denied");
        }
    }

    Ok(())
}

/// Truncate output to prevent context bloat
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...\n[truncated, {} total chars]", &s[..max], s.len())
    }
}

// ── Shell tool ──────────────────────────────────────────────────────

struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str { "Run a shell command: {\"tool\": \"shell\", \"params\": {\"command\": \"...\"}}" }
    fn needs_confirmation(&self) -> bool { false } // Handled by sensitive pattern check in strategy

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let cmd = params.get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("shell: missing 'command' param".into()))?;

        let output = session.exec(cmd).await?;
        Ok(ToolResult {
            success: true,
            output: truncate(&output, MAX_OUTPUT_LEN),
        })
    }
}

// ── FileRead tool ───────────────────────────────────────────────────

struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str { "file_read" }
    fn description(&self) -> &str { "Read a file: {\"tool\": \"file_read\", \"params\": {\"path\": \"...\"}}" }
    fn needs_confirmation(&self) -> bool { false }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_read: missing 'path' param".into()))?;

        if let Err(reason) = validate_path(path) {
            return Ok(ToolResult {
                success: false,
                output: reason.into(),
            });
        }

        let cmd = format!("cat '{}'", path.replace('\'', "'\\''"));
        let output = session.exec(&cmd).await?;
        Ok(ToolResult {
            success: true,
            output: truncate(&output, MAX_OUTPUT_LEN),
        })
    }
}

// ── FileWrite tool ──────────────────────────────────────────────────

struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str { "file_write" }
    fn description(&self) -> &str { "Write a file: {\"tool\": \"file_write\", \"params\": {\"path\": \"...\", \"content\": \"...\"}}" }
    fn needs_confirmation(&self) -> bool { true }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_write: missing 'path' param".into()))?;
        let content = params.get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_write: missing 'content' param".into()))?;

        if let Err(reason) = validate_path(path) {
            return Ok(ToolResult {
                success: false,
                output: reason.into(),
            });
        }

        let write_cmd = format!("cat > '{}'", path.replace('\'', "'\\''"));
        session.exec_with_stdin(&write_cmd, content).await?;

        Ok(ToolResult {
            success: true,
            output: format!("Wrote {} bytes to {}", content.len(), path),
        })
    }
}

// ── Registry ────────────────────────────────────────────────────────

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Build a registry scoped to a ghost's allowed tools
    pub fn for_ghost(ghost: &GhostConfig) -> Self {
        let all_tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ShellTool),
            Box::new(FileReadTool),
            Box::new(FileWriteTool),
        ];

        let tools: HashMap<String, Box<dyn Tool>> = all_tools
            .into_iter()
            .filter(|t| ghost.tools.contains(&t.name().to_string()))
            .map(|t| (t.name().to_string(), t))
            .collect();

        Self { tools }
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Format tool descriptions for the LLM system prompt
    pub fn descriptions(&self) -> String {
        self.tools.values()
            .map(|t| format!("- {}", t.description()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }
}
