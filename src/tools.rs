use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::config::GhostConfig;
use crate::docker::DockerSession;
use crate::dynamic_tools;
use crate::error::{AthenaError, Result};
use crate::llm::ToolSchema;

const MAX_OUTPUT_LEN: usize = 2000;
const SEARCH_OUTPUT_LEN: usize = 8000;
const GLOB_OUTPUT_LEN: usize = 4000;

#[derive(Debug)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn needs_confirmation(&self) -> bool;
    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult>;

    /// JSON Schema for this tool's parameters. Used for native function calling.
    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        })
    }
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

/// Validate a URL for safety: must be http(s), no private/internal IPs (SSRF protection)
fn validate_url(url: &str) -> std::result::Result<(), &'static str> {
    // Must start with http:// or https://
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("URL must use http:// or https:// scheme");
    }

    // Extract host:port from URL (everything between :// and first /)
    let authority = url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");

    // Handle bracketed IPv6: [::1] or [::1]:8080
    let host = if authority.starts_with('[') {
        // IPv6 bracketed — extract content between [ and ]
        authority
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        // IPv4 or hostname — split on : to strip port
        authority.split(':').next().unwrap_or("")
    };

    let host_lower = host.to_lowercase();

    // Block localhost
    if host_lower == "localhost" || host_lower == "127.0.0.1" || host_lower == "0.0.0.0" {
        return Err("Access to localhost denied");
    }

    // Block IPv6 loopback
    if let Ok(ip6) = host.parse::<std::net::Ipv6Addr>() {
        if ip6.is_loopback() {
            return Err("Access to localhost denied");
        }
    }

    // Block private IP ranges (10.x.x.x, 172.16-31.x.x, 192.168.x.x, 169.254.x.x)
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let octets = ip.octets();
        if octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168)
            || (octets[0] == 169 && octets[1] == 254)
        {
            return Err("Access to private IP ranges denied");
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
    fn description(&self) -> String { "Run a shell command: {\"tool\": \"shell\", \"params\": {\"command\": \"...\"}}".into() }
    fn needs_confirmation(&self) -> bool { false } // Handled by sensitive pattern check in strategy

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" }
            },
            "required": ["command"]
        })
    }

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
    fn description(&self) -> String { "Read a file: {\"tool\": \"file_read\", \"params\": {\"path\": \"...\"}}".into() }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read" }
            },
            "required": ["path"]
        })
    }

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
    fn description(&self) -> String { "Write a file: {\"tool\": \"file_write\", \"params\": {\"path\": \"...\", \"content\": \"...\"}}".into() }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

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

// ── FileEdit tool ───────────────────────────────────────────────────

struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str { "file_edit" }
    fn description(&self) -> String {
        "Edit a file by replacing a string: {\"tool\": \"file_edit\", \"params\": {\"path\": \"...\", \"old_string\": \"...\", \"new_string\": \"...\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit" },
                "old_string": { "type": "string", "description": "The exact string to find and replace (must be unique in the file)" },
                "new_string": { "type": "string", "description": "The replacement string" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_edit: missing 'path' param".into()))?;
        let old_string = params.get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_edit: missing 'old_string' param".into()))?;
        let new_string = params.get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("file_edit: missing 'new_string' param".into()))?;

        if let Err(reason) = validate_path(path) {
            return Ok(ToolResult {
                success: false,
                output: reason.into(),
            });
        }

        // Read the file
        let cat_cmd = format!("cat '{}'", path.replace('\'', "'\\''"));
        let content = session.exec(&cat_cmd).await?;

        // Check that old_string exists
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolResult {
                success: false,
                output: format!("file_edit: '{}' not found in {}", old_string, path),
            });
        }
        if count > 1 {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "file_edit: '{}' found {} times in {} (must be unique, provide more context)",
                    old_string, count, path
                ),
            });
        }

        // Replace (exactly one match)
        let new_content = content.replacen(old_string, new_string, 1);

        // Write back
        let write_cmd = format!("cat > '{}'", path.replace('\'', "'\\''"));
        session.exec_with_stdin(&write_cmd, &new_content).await?;

        Ok(ToolResult {
            success: true,
            output: truncate(
                &format!("Edited {}:\n- {}\n+ {}", path, old_string, new_string),
                MAX_OUTPUT_LEN,
            ),
        })
    }
}

// ── Grep tool ───────────────────────────────────────────────────────

struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> String {
        "Search file contents: {\"tool\": \"grep\", \"params\": {\"pattern\": \"...\", \"path\": \".\", \"include\": \"*.rs\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Directory or file to search in (default: \".\")" },
                "include": { "type": "string", "description": "File glob pattern to filter (e.g. \"*.rs\")" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let pattern = params.get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("grep: missing 'pattern' param".into()))?;
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let include = params.get("include")
            .and_then(|v| v.as_str());

        // Validate search path
        if path != "." {
            if let Err(reason) = validate_path(path) {
                return Ok(ToolResult {
                    success: false,
                    output: reason.into(),
                });
            }
        }

        // Build grep command: -r recursive, -n line numbers
        // Shell-escape the pattern by using -- to end options
        let escaped_path = path.replace('\'', "'\\''");
        let escaped_pattern = pattern.replace('\'', "'\\''");

        let mut cmd = format!("grep -rn -- '{}' '{}'", escaped_pattern, escaped_path);

        if let Some(inc) = include {
            let escaped_inc = inc.replace('\'', "'\\''");
            cmd = format!("grep -rn --include='{}' -- '{}' '{}'", escaped_inc, escaped_pattern, escaped_path);
        }

        // Limit output lines
        cmd = format!("{} | head -50", cmd);

        let output = session.exec(&cmd).await?;
        if output.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No matches for '{}' in {}", pattern, path),
            });
        }

        Ok(ToolResult {
            success: true,
            output: truncate(&output, SEARCH_OUTPUT_LEN),
        })
    }
}

// ── Glob tool ───────────────────────────────────────────────────────

struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> String {
        "Find files by pattern: {\"tool\": \"glob\", \"params\": {\"pattern\": \"*.rs\", \"path\": \".\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Filename glob pattern (e.g. \"*.rs\", \"**/*.py\")" },
                "path": { "type": "string", "description": "Directory to search in (default: \".\")" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let pattern = params.get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("glob: missing 'pattern' param".into()))?;
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        // Validate search path
        if path != "." {
            if let Err(reason) = validate_path(path) {
                return Ok(ToolResult {
                    success: false,
                    output: reason.into(),
                });
            }
        }

        // Extract just the filename pattern (e.g., "**/*.rs" -> "*.rs")
        let name_pattern = pattern.rsplit('/').next().unwrap_or(pattern);

        let escaped_path = path.replace('\'', "'\\''");
        let escaped_name = name_pattern.replace('\'', "'\\''");

        let cmd = format!(
            "find '{}' -name '{}' -type f 2>/dev/null | head -100 | sort",
            escaped_path, escaped_name
        );

        let output = session.exec(&cmd).await?;
        if output.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No files matching '{}' in {}", pattern, path),
            });
        }

        Ok(ToolResult {
            success: true,
            output: truncate(&output, GLOB_OUTPUT_LEN),
        })
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

/// Strip HTML tags, decode common entities, and collapse whitespace
fn strip_html(html: &str) -> String {
    let re_tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re_tags.replace_all(html, "");

    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    let re_ws = regex::Regex::new(r"\s+").unwrap();
    let text = re_ws.replace_all(&text, " ");
    text.trim().to_string()
}

/// Minimal percent-decoding for DuckDuckGo redirect URLs
fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                result.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── WebFetch tool ───────────────────────────────────────────────────

struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Athena/0.1")
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str { "web_fetch" }
    fn description(&self) -> String {
        "Fetch a URL: {\"tool\": \"web_fetch\", \"params\": {\"url\": \"https://...\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch (must be http or https)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, _session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let url = params.get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("web_fetch: missing 'url' param".into()))?;

        if let Err(reason) = validate_url(url) {
            return Ok(ToolResult {
                success: false,
                output: reason.into(),
            });
        }

        let response = self.client
            .get(url)
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("web_fetch: request failed: {}", e)))?;

        let status = response.status();
        if !status.is_success() {
            return Ok(ToolResult {
                success: false,
                output: format!("web_fetch: HTTP {}", status),
            });
        }

        // Limit body size to 1MB
        let bytes = response
            .bytes()
            .await
            .map_err(|e| AthenaError::Tool(format!("web_fetch: read failed: {}", e)))?;

        if bytes.len() > 1_048_576 {
            return Ok(ToolResult {
                success: false,
                output: "web_fetch: response too large (>1MB)".into(),
            });
        }

        let body = String::from_utf8_lossy(&bytes).to_string();
        let text = strip_html(&body);

        Ok(ToolResult {
            success: true,
            output: truncate(&text, SEARCH_OUTPUT_LEN),
        })
    }
}

// ── WebSearch tool ──────────────────────────────────────────────────

struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Athena/0.1")
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str { "web_search" }
    fn description(&self) -> String {
        "Search the web: {\"tool\": \"web_search\", \"params\": {\"query\": \"...\", \"num_results\": 5}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "num_results": { "type": "integer", "description": "Number of results to return (default: 5, max: 10)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, _session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let query = params.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AthenaError::Tool("web_search: missing 'query' param".into()))?;
        let num_results = params.get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(10) as usize;

        let response = self.client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("web_search: request failed: {}", e)))?;

        if !response.status().is_success() {
            return Ok(ToolResult {
                success: false,
                output: format!("web_search: HTTP {}", response.status()),
            });
        }

        let body = response
            .text()
            .await
            .map_err(|e| AthenaError::Tool(format!("web_search: read failed: {}", e)))?;

        // Parse results from DuckDuckGo HTML
        let re_result = regex::Regex::new(
            r#"class="result__a"[^>]*href="([^"]*)"[^>]*>([^<]*)</a>"#
        ).unwrap();
        let re_snippet = regex::Regex::new(
            r#"class="result__snippet"[^>]*>(.*?)</(?:td|a|span|div)>"#
        ).unwrap();

        let titles: Vec<(&str, &str)> = re_result.captures_iter(&body)
            .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
            .collect();
        let snippets: Vec<&str> = re_snippet.captures_iter(&body)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();

        if titles.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No results found for '{}'", query),
            });
        }

        let mut output = String::new();
        for (i, (raw_url, title)) in titles.iter().enumerate().take(num_results) {
            // Extract actual URL from DuckDuckGo redirect
            let url = if raw_url.contains("uddg=") {
                raw_url
                    .split("uddg=")
                    .nth(1)
                    .unwrap_or(raw_url)
                    .split('&')
                    .next()
                    .map(|u| percent_decode(u))
                    .unwrap_or_else(|| raw_url.to_string())
            } else {
                raw_url.to_string()
            };

            let clean_title = strip_html(title);
            let snippet = snippets.get(i)
                .map(|s| strip_html(s))
                .unwrap_or_default();

            output.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, clean_title, url, snippet));
        }

        Ok(ToolResult {
            success: true,
            output: truncate(output.trim(), SEARCH_OUTPUT_LEN),
        })
    }
}

// ── CodebaseMap tool ────────────────────────────────────────────────

struct CodebaseMapTool;

#[async_trait]
impl Tool for CodebaseMapTool {
    fn name(&self) -> &str { "codebase_map" }
    fn description(&self) -> String {
        "Show project structure and key symbols: {\"tool\": \"codebase_map\", \"params\": {\"path\": \".\", \"depth\": 3}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Root directory to map (default: \".\")" },
                "depth": { "type": "integer", "description": "Max directory depth (default: 3, max: 10)" }
            }
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let depth = params.get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .max(1)
            .min(10);

        if path != "." {
            if let Err(reason) = validate_path(path) {
                return Ok(ToolResult {
                    success: false,
                    output: reason.into(),
                });
            }
        }

        let escaped_path = path.replace('\'', "'\\''");

        // File tree excluding common noise directories
        let tree_cmd = format!(
            "find '{}' -maxdepth {} -type f \
             ! -path '*/.git/*' ! -path '*/target/*' ! -path '*/node_modules/*' ! -path '*/__pycache__/*' \
             2>/dev/null | head -200 | sort",
            escaped_path, depth
        );
        let tree = session.exec(&tree_cmd).await?;

        if tree.trim().is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No files found in '{}'", path),
            });
        }

        // Detect languages from file extensions in tree
        let has_rs = tree.contains(".rs");
        let has_py = tree.contains(".py");
        let has_js = tree.contains(".js") || tree.contains(".ts") || tree.contains(".tsx");
        let has_go = tree.contains(".go");

        let mut symbols = String::new();

        if has_rs {
            let cmd = format!(
                "grep -rn --include='*.rs' -E '^\\s*(pub\\s+)?(fn|struct|enum|trait|impl|mod)\\s' '{}' 2>/dev/null | head -80",
                escaped_path
            );
            let out = session.exec(&cmd).await?;
            if !out.trim().is_empty() {
                symbols.push_str("Rust:\n");
                symbols.push_str(&out);
                symbols.push('\n');
            }
        }
        if has_py {
            let cmd = format!(
                "grep -rn --include='*.py' -E '^(def |class )' '{}' 2>/dev/null | head -80",
                escaped_path
            );
            let out = session.exec(&cmd).await?;
            if !out.trim().is_empty() {
                symbols.push_str("Python:\n");
                symbols.push_str(&out);
                symbols.push('\n');
            }
        }
        if has_js {
            let cmd = format!(
                "grep -rn --include='*.js' --include='*.ts' --include='*.tsx' -E '^(export |function |class )' '{}' 2>/dev/null | head -80",
                escaped_path
            );
            let out = session.exec(&cmd).await?;
            if !out.trim().is_empty() {
                symbols.push_str("JS/TS:\n");
                symbols.push_str(&out);
                symbols.push('\n');
            }
        }
        if has_go {
            let cmd = format!(
                "grep -rn --include='*.go' -E '^(func |type .*(struct|interface))' '{}' 2>/dev/null | head -80",
                escaped_path
            );
            let out = session.exec(&cmd).await?;
            if !out.trim().is_empty() {
                symbols.push_str("Go:\n");
                symbols.push_str(&out);
                symbols.push('\n');
            }
        }

        let mut output = format!("FILE TREE:\n{}", tree);
        if !symbols.is_empty() {
            output.push_str(&format!("\nKEY SYMBOLS:\n{}", symbols));
        }

        Ok(ToolResult {
            success: true,
            output: truncate(&output, SEARCH_OUTPUT_LEN),
        })
    }
}

// ── Lint tool ───────────────────────────────────────────────────────

struct LintTool;

/// Detect lint command from file extension
fn detect_lint_command(path: &str) -> String {
    let escaped = path.replace('\'', "'\\''");
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match ext {
        "rs" => "test -f Cargo.toml && cargo check 2>&1 || echo 'No Cargo.toml found'".to_string(),
        "py" => format!("python3 -m py_compile '{}' 2>&1 && echo 'OK: no syntax errors'", escaped),
        "js" | "jsx" => format!("node --check '{}' 2>&1 && echo 'OK: no syntax errors'", escaped),
        "ts" | "tsx" => format!(
            "if command -v tsc >/dev/null; then tsc --noEmit '{}' 2>&1; else node --check '{}' 2>&1; fi",
            escaped, escaped
        ),
        "go" => "go vet ./... 2>&1".to_string(),
        _ => format!("echo 'No linter available for .{} files'", ext),
    }
}

#[async_trait]
impl Tool for LintTool {
    fn name(&self) -> &str { "lint" }
    fn description(&self) -> String {
        "Check code for errors: {\"tool\": \"lint\", \"params\": {\"path\": \"src/main.rs\"}} or {\"tool\": \"lint\", \"params\": {\"command\": \"cargo check\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to lint (auto-detects language)" },
                "command": { "type": "string", "description": "Explicit lint command to run (overrides path)" }
            }
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path").and_then(|v| v.as_str());
        let command = params.get("command").and_then(|v| v.as_str());

        let cmd = match (command, path) {
            (Some(c), _) => format!("{} 2>&1", c),
            (None, Some(p)) => {
                if let Err(reason) = validate_path(p) {
                    return Ok(ToolResult {
                        success: false,
                        output: reason.into(),
                    });
                }
                detect_lint_command(p)
            }
            (None, None) => {
                return Ok(ToolResult {
                    success: false,
                    output: "lint: provide either 'path' or 'command' param".into(),
                });
            }
        };

        let output = session.exec(&cmd).await?;

        Ok(ToolResult {
            success: true,
            output: truncate(&output, SEARCH_OUTPUT_LEN),
        })
    }
}

// ── Diff tool ───────────────────────────────────────────────────────

struct DiffTool;

#[async_trait]
impl Tool for DiffTool {
    fn name(&self) -> &str { "diff" }
    fn description(&self) -> String {
        "Show git changes: {\"tool\": \"diff\", \"params\": {\"path\": \"src/main.rs\"}} or {\"tool\": \"diff\", \"params\": {}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    fn parameter_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File or directory to diff (default: all files)" }
            }
        })
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let path = params.get("path").and_then(|v| v.as_str());

        // Check if inside a git repo
        let check = session.exec("git rev-parse --is-inside-work-tree 2>&1").await?;
        if !check.trim().eq_ignore_ascii_case("true") {
            return Ok(ToolResult {
                success: false,
                output: "Not a git repository".into(),
            });
        }

        if let Some(p) = path {
            if p != "." {
                if let Err(reason) = validate_path(p) {
                    return Ok(ToolResult {
                        success: false,
                        output: reason.into(),
                    });
                }
            }
        }

        let target = path.unwrap_or("all files");
        let cmd = match path {
            Some(p) => {
                let escaped = p.replace('\'', "'\\''");
                format!("git diff HEAD -- '{}' 2>&1", escaped)
            }
            None => "git diff HEAD 2>&1".to_string(),
        };

        let output = session.exec(&cmd).await?;

        if output.trim().is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No changes in {}", target),
            });
        }

        Ok(ToolResult {
            success: true,
            output: truncate(&output, SEARCH_OUTPUT_LEN),
        })
    }
}

// ── Coding CLI tools (host-executed) ────────────────────────────────

const CLI_OUTPUT_LEN: usize = 16_000;
const CLI_TIMEOUT_SECS: u64 = 3600; // 1 hour

/// Shared implementation for all coding CLI tools.
/// Runs on the HOST (not in Docker) via tokio::process::Command.
async fn run_cli_tool(
    command: &str,
    args: &[&str],
    workspace: &str,
    tool_name: &str,
) -> Result<ToolResult> {
    use tokio::process::Command;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(CLI_TIMEOUT_SECS),
        Command::new(command)
            .args(args)
            .current_dir(workspace)
            .env("TERM", "dumb") // avoid ANSI escape codes
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let combined = if stderr.is_empty() {
                stdout
            } else {
                format!("{}\n[stderr]\n{}", stdout, stderr)
            };
            Ok(ToolResult {
                success: output.status.success(),
                output: truncate(&combined, CLI_OUTPUT_LEN),
            })
        }
        Ok(Err(e)) => {
            // Command failed to start (not installed, permission denied, etc.)
            Ok(ToolResult {
                success: false,
                output: format!("{}: command failed — {}. Is it installed and in PATH?", tool_name, e),
            })
        }
        Err(_) => {
            Ok(ToolResult {
                success: false,
                output: format!("{}: timed out after {}s", tool_name, CLI_TIMEOUT_SECS),
            })
        }
    }
}

/// Build an enriched prompt for CLI coding tools.
/// Prepends optional context the ghost provides (files it read, constraints, etc.)
/// so the coding agent starts with full awareness.
fn build_cli_prompt(params: &Value) -> std::result::Result<String, AthenaError> {
    let prompt = params.get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AthenaError::Tool("missing 'prompt' param".into()))?;

    let context = params.get("context").and_then(|v| v.as_str());
    let files = params.get("files").and_then(|v| v.as_str()); // optional: key file contents

    let mut full = String::new();
    if let Some(ctx) = context {
        full.push_str("CONTEXT:\n");
        full.push_str(ctx);
        full.push_str("\n\n");
    }
    if let Some(f) = files {
        full.push_str("RELEVANT FILES:\n");
        full.push_str(f);
        full.push_str("\n\n");
    }
    full.push_str("TASK:\n");
    full.push_str(prompt);
    Ok(full)
}

struct ClaudeCodeTool { workspace: String }

impl ClaudeCodeTool {
    fn new(workspace: &str) -> Self {
        Self { workspace: workspace.to_string() }
    }
}

#[async_trait]
impl Tool for ClaudeCodeTool {
    fn name(&self) -> &str { "claude_code" }
    fn description(&self) -> String {
        "Run Claude Code to implement a coding task (full agent with file editing, compilation, tests): {\"tool\": \"claude_code\", \"params\": {\"prompt\": \"...\", \"context\": \"(optional background)\", \"files\": \"(optional file contents)\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    async fn execute(&self, _session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let prompt = build_cli_prompt(params)?;
        run_cli_tool("claude", &["-p", &prompt, "--output-format", "text", "--dangerously-skip-permissions"], &self.workspace, "claude_code").await
    }
}

struct CodexTool { workspace: String }

impl CodexTool {
    fn new(workspace: &str) -> Self {
        Self { workspace: workspace.to_string() }
    }
}

#[async_trait]
impl Tool for CodexTool {
    fn name(&self) -> &str { "codex" }
    fn description(&self) -> String {
        "Run OpenAI Codex CLI to implement a coding task (full agent with file editing): {\"tool\": \"codex\", \"params\": {\"prompt\": \"...\", \"context\": \"(optional background)\", \"files\": \"(optional file contents)\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    async fn execute(&self, _session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let prompt = build_cli_prompt(params)?;
        run_cli_tool("codex", &["exec", "--full-auto", &prompt], &self.workspace, "codex").await
    }
}

struct OpenCodeTool { workspace: String }

impl OpenCodeTool {
    fn new(workspace: &str) -> Self {
        Self { workspace: workspace.to_string() }
    }
}

#[async_trait]
impl Tool for OpenCodeTool {
    fn name(&self) -> &str { "opencode" }
    fn description(&self) -> String {
        "Run OpenCode CLI to implement a coding task (full agent with file editing): {\"tool\": \"opencode\", \"params\": {\"prompt\": \"...\", \"context\": \"(optional background)\", \"files\": \"(optional file contents)\"}}".into()
    }
    fn needs_confirmation(&self) -> bool { false }

    async fn execute(&self, _session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let prompt = build_cli_prompt(params)?;
        run_cli_tool("opencode", &["run", &prompt], &self.workspace, "opencode").await
    }
}

// ── Registry ────────────────────────────────────────────────────────

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Build a registry scoped to a ghost's allowed tools.
    /// If `dynamic_tools_path` is provided, also loads YAML-defined tools from that directory.
    pub fn for_ghost(ghost: &GhostConfig, dynamic_tools_path: Option<&Path>) -> Self {
        // Resolve host workspace for CLI tools (first writable mount, or ".")
        let host_workspace = ghost.mounts.iter()
            .find(|m| !m.read_only)
            .map(|m| m.host_path.clone())
            .unwrap_or_else(|| ".".to_string());

        let mut all_tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ShellTool),
            Box::new(FileReadTool),
            Box::new(FileWriteTool),
            Box::new(FileEditTool),
            Box::new(GrepTool),
            Box::new(GlobTool),
            Box::new(WebFetchTool::new()),
            Box::new(WebSearchTool::new()),
            Box::new(CodebaseMapTool),
            Box::new(LintTool),
            Box::new(DiffTool),
            Box::new(ClaudeCodeTool::new(&host_workspace)),
            Box::new(CodexTool::new(&host_workspace)),
            Box::new(OpenCodeTool::new(&host_workspace)),
        ];

        // Load dynamic tools from YAML definitions
        if let Some(path) = dynamic_tools_path {
            match dynamic_tools::discover(path) {
                Ok(dynamic) => {
                    tracing::info!("Discovered {} dynamic tool(s) from {}", dynamic.len(), path.display());
                    all_tools.extend(dynamic);
                }
                Err(e) => {
                    tracing::warn!("Failed to discover dynamic tools: {}", e);
                }
            }
        }

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

    /// Generate `ToolSchema` definitions for all registered tools (for native function calling).
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description(),
                parameters: t.parameter_schema(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_html ──────────────────────────────────────────────────

    #[test]
    fn test_strip_html_removes_tags() {
        assert_eq!(strip_html("<b>bold</b> text"), "bold text");
    }

    #[test]
    fn test_strip_html_decodes_entities() {
        assert_eq!(strip_html("a &amp; b &lt; c &gt; d"), "a & b < c > d");
        assert_eq!(strip_html("&quot;hello&quot; &#39;world&#39;"), "\"hello\" 'world'");
        assert_eq!(strip_html("non&nbsp;breaking"), "non breaking");
    }

    #[test]
    fn test_strip_html_collapses_whitespace() {
        assert_eq!(strip_html("hello   \n\t  world"), "hello world");
    }

    #[test]
    fn test_strip_html_complex() {
        let html = r#"<div class="result"><a href="x">Title</a><span>some &amp; text</span></div>"#;
        assert_eq!(strip_html(html), "Titlesome & text");
    }

    #[test]
    fn test_strip_html_empty() {
        assert_eq!(strip_html(""), "");
        assert_eq!(strip_html("   "), "");
    }

    #[test]
    fn test_strip_html_no_html() {
        assert_eq!(strip_html("plain text"), "plain text");
    }

    // ── percent_decode ──────────────────────────────────────────────

    #[test]
    fn test_percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn test_percent_decode_url() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fpath%3Fq%3Dtest"),
            "https://example.com/path?q=test"
        );
    }

    #[test]
    fn test_percent_decode_no_encoding() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn test_percent_decode_mixed_case() {
        assert_eq!(percent_decode("%2f%2F"), "//");
    }

    #[test]
    fn test_percent_decode_invalid_sequence() {
        // Invalid hex chars — should pass through literal %
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        // Truncated sequence at end
        assert_eq!(percent_decode("abc%2"), "abc%2");
        assert_eq!(percent_decode("abc%"), "abc%");
    }

    #[test]
    fn test_percent_decode_empty() {
        assert_eq!(percent_decode(""), "");
    }

    // ── hex_val ─────────────────────────────────────────────────────

    #[test]
    fn test_hex_val_digits() {
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
    }

    #[test]
    fn test_hex_val_lowercase() {
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'f'), Some(15));
    }

    #[test]
    fn test_hex_val_uppercase() {
        assert_eq!(hex_val(b'A'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
    }

    #[test]
    fn test_hex_val_invalid() {
        assert_eq!(hex_val(b'g'), None);
        assert_eq!(hex_val(b'z'), None);
        assert_eq!(hex_val(b' '), None);
    }

    // ── validate_path ───────────────────────────────────────────────

    #[test]
    fn test_validate_path_ok() {
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("file.txt").is_ok());
        assert!(validate_path("/workspace/src/main.rs").is_ok());
    }

    #[test]
    fn test_validate_path_traversal() {
        assert_eq!(validate_path("../etc/passwd"), Err("Path traversal (..) not allowed"));
        assert_eq!(validate_path("src/../../secret"), Err("Path traversal (..) not allowed"));
    }

    #[test]
    fn test_validate_path_absolute_outside_workspace() {
        assert_eq!(validate_path("/etc/passwd"), Err("Absolute paths must be under /workspace"));
        assert_eq!(validate_path("/tmp/file"), Err("Absolute paths must be under /workspace"));
    }

    #[test]
    fn test_validate_path_sensitive_files() {
        assert_eq!(validate_path(".env"), Err("Access to sensitive file denied"));
        assert_eq!(validate_path("src/.env.local"), Err("Access to sensitive file denied"));
        assert_eq!(validate_path("config.toml"), Err("Access to sensitive file denied"));
        assert_eq!(validate_path("credentials.json"), Err("Access to sensitive file denied"));
        assert_eq!(validate_path("secrets.toml"), Err("Access to sensitive file denied"));
    }

    #[test]
    fn test_validate_path_sensitive_extensions() {
        assert_eq!(validate_path("server.pem"), Err("Access to sensitive file type denied"));
        assert_eq!(validate_path("private.key"), Err("Access to sensitive file type denied"));
    }

    // ── validate_url ────────────────────────────────────────────────

    #[test]
    fn test_validate_url_ok() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://example.com/path?q=1").is_ok());
    }

    #[test]
    fn test_validate_url_bad_scheme() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn test_validate_url_localhost() {
        assert!(validate_url("http://localhost").is_err());
        assert!(validate_url("http://127.0.0.1").is_err());
        assert!(validate_url("http://0.0.0.0").is_err());
        assert!(validate_url("http://[::1]").is_err());
        assert!(validate_url("http://[::1]:8080").is_err());
        assert!(validate_url("http://[::1]:8080/path").is_err());
    }

    #[test]
    fn test_validate_url_private_ips() {
        assert!(validate_url("http://10.0.0.1").is_err());
        assert!(validate_url("http://172.16.0.1").is_err());
        assert!(validate_url("http://172.31.255.255").is_err());
        assert!(validate_url("http://192.168.1.1").is_err());
        assert!(validate_url("http://169.254.169.254").is_err()); // AWS metadata
    }

    #[test]
    fn test_validate_url_allowed_private_adjacent() {
        // 172.15.x.x is NOT private
        assert!(validate_url("http://172.15.0.1").is_ok());
        // 172.32.x.x is NOT private
        assert!(validate_url("http://172.32.0.1").is_ok());
    }

    // ── truncate ────────────────────────────────────────────────────

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        let result = truncate("hello world", 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("[truncated, 11 total chars]"));
    }

    // ── detect_lint_command ─────────────────────────────────────────

    #[test]
    fn test_detect_lint_rust() {
        let cmd = detect_lint_command("src/main.rs");
        assert!(cmd.contains("cargo check"));
    }

    #[test]
    fn test_detect_lint_python() {
        let cmd = detect_lint_command("script.py");
        assert!(cmd.contains("py_compile"));
        assert!(cmd.contains("script.py"));
    }

    #[test]
    fn test_detect_lint_javascript() {
        let cmd = detect_lint_command("app.js");
        assert!(cmd.contains("node --check"));
        assert!(cmd.contains("app.js"));
    }

    #[test]
    fn test_detect_lint_jsx() {
        let cmd = detect_lint_command("Component.jsx");
        assert!(cmd.contains("node --check"));
    }

    #[test]
    fn test_detect_lint_typescript() {
        let cmd = detect_lint_command("app.ts");
        assert!(cmd.contains("tsc --noEmit") || cmd.contains("node --check"));
    }

    #[test]
    fn test_detect_lint_tsx() {
        let cmd = detect_lint_command("Component.tsx");
        assert!(cmd.contains("tsc") || cmd.contains("node --check"));
    }

    #[test]
    fn test_detect_lint_go() {
        let cmd = detect_lint_command("main.go");
        assert!(cmd.contains("go vet"));
    }

    #[test]
    fn test_detect_lint_unknown() {
        let cmd = detect_lint_command("data.csv");
        assert!(cmd.contains("No linter available for .csv"));
    }

    #[test]
    fn test_detect_lint_no_extension() {
        let cmd = detect_lint_command("Makefile");
        assert!(cmd.contains("No linter available"));
    }

    #[test]
    fn test_detect_lint_shell_escape() {
        // Path with single quotes should be escaped
        let cmd = detect_lint_command("it's a file.py");
        assert!(cmd.contains("py_compile"));
        assert!(cmd.contains("'\\''"));
    }

    // ── Tool registry / ghost scoping ───────────────────────────────

    fn make_ghost(tools: Vec<&str>) -> GhostConfig {
        GhostConfig {
            name: "test".into(),
            description: "test ghost".into(),
            tools: tools.into_iter().map(String::from).collect(),
            mounts: vec![],
            strategy: "react".into(),
            soul_file: None,
            soul: None,
            image: None,
        }
    }

    #[test]
    fn test_registry_coder_tools() {
        let ghost = make_ghost(vec![
            "file_read", "file_write", "file_edit", "shell",
            "grep", "glob", "web_fetch", "codebase_map",
            "web_search", "lint", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        let names = reg.tool_names();
        assert_eq!(names.len(), 11);
        assert!(reg.get("codebase_map").is_some());
        assert!(reg.get("web_search").is_some());
        assert!(reg.get("lint").is_some());
        assert!(reg.get("diff").is_some());
    }

    #[test]
    fn test_registry_scout_tools() {
        let ghost = make_ghost(vec![
            "file_read", "shell", "grep", "glob",
            "codebase_map", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        let names = reg.tool_names();
        assert_eq!(names.len(), 6);
        assert!(reg.get("codebase_map").is_some());
        assert!(reg.get("diff").is_some());
        // Scout should NOT have these
        assert!(reg.get("lint").is_none());
        assert!(reg.get("web_search").is_none());
        assert!(reg.get("file_write").is_none());
        assert!(reg.get("file_edit").is_none());
        assert!(reg.get("web_fetch").is_none());
    }

    #[test]
    fn test_registry_filters_unknown_tools() {
        let ghost = make_ghost(vec!["shell", "nonexistent_tool"]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        assert_eq!(reg.tool_names().len(), 1);
        assert!(reg.get("shell").is_some());
        assert!(reg.get("nonexistent_tool").is_none());
    }

    #[test]
    fn test_registry_empty_tools() {
        let ghost = make_ghost(vec![]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        assert_eq!(reg.tool_names().len(), 0);
    }

    #[test]
    fn test_registry_all_11_tools_available() {
        let ghost = make_ghost(vec![
            "shell", "file_read", "file_write", "file_edit",
            "grep", "glob", "web_fetch", "web_search",
            "codebase_map", "lint", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        assert_eq!(reg.tool_names().len(), 11);
    }

    // ── Tool metadata ───────────────────────────────────────────────

    #[test]
    fn test_tool_names_match() {
        let ghost = make_ghost(vec![
            "shell", "file_read", "file_write", "file_edit",
            "grep", "glob", "web_fetch", "web_search",
            "codebase_map", "lint", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        // Every registered tool name must match what the tool reports
        for name in reg.tool_names() {
            let tool = reg.get(name).unwrap();
            assert_eq!(tool.name(), name);
        }
    }

    #[test]
    fn test_tool_descriptions_non_empty() {
        let ghost = make_ghost(vec![
            "shell", "file_read", "file_write", "file_edit",
            "grep", "glob", "web_fetch", "web_search",
            "codebase_map", "lint", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);
        let desc = reg.descriptions();
        assert!(!desc.is_empty());
        // Each tool should have a description line
        for name in reg.tool_names() {
            let tool = reg.get(name).unwrap();
            assert!(!tool.description().is_empty(), "Tool {} has empty description", name);
            assert!(tool.description().contains("tool"), "Tool {} description missing 'tool' keyword", name);
        }
    }

    #[test]
    fn test_confirmation_gates() {
        let ghost = make_ghost(vec![
            "shell", "file_read", "file_write", "file_edit",
            "grep", "glob", "web_fetch", "web_search",
            "codebase_map", "lint", "diff",
        ]);
        let reg = ToolRegistry::for_ghost(&ghost, None);

        // No tools require confirmation (Docker sandbox is the safety boundary)
        assert!(!reg.get("file_write").unwrap().needs_confirmation());
        assert!(!reg.get("file_edit").unwrap().needs_confirmation());

        // All Phase 2 tools are read-only — no confirmation
        assert!(!reg.get("codebase_map").unwrap().needs_confirmation());
        assert!(!reg.get("web_search").unwrap().needs_confirmation());
        assert!(!reg.get("lint").unwrap().needs_confirmation());
        assert!(!reg.get("diff").unwrap().needs_confirmation());

        // Phase 1 read-only tools
        assert!(!reg.get("shell").unwrap().needs_confirmation());
        assert!(!reg.get("file_read").unwrap().needs_confirmation());
        assert!(!reg.get("grep").unwrap().needs_confirmation());
        assert!(!reg.get("glob").unwrap().needs_confirmation());
        assert!(!reg.get("web_fetch").unwrap().needs_confirmation());
    }
}
