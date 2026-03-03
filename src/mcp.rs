use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::config::{McpConfig, McpServerConfig, McpTransport};
use crate::error::{AthenaError, Result};
use crate::observer::{ObserverCategory, ObserverHandle};

const INIT_PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "athena";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const SHUTDOWN_GRACE_MS: u64 = 750;

#[derive(Debug, Clone)]
pub struct DiscoveredMcpTool {
    pub server: String,
    pub remote_name: String,
    pub namespaced_name: String,
    pub description: String,
    pub input_schema: Value,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone)]
pub struct McpInvocationResult {
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpFailureKind {
    Connection,
    Auth,
    Discovery,
    Invocation,
    Timeout,
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerHealth {
    Unknown,
    Ready,
    Degraded,
}

#[derive(Debug, Clone)]
struct ServerRuntimeState {
    health: ServerHealth,
    tools: Vec<DiscoveredMcpTool>,
    last_discovery: Option<Instant>,
    last_error: Option<String>,
}

impl Default for ServerRuntimeState {
    fn default() -> Self {
        Self {
            health: ServerHealth::Unknown,
            tools: Vec::new(),
            last_discovery: None,
            last_error: None,
        }
    }
}

struct ServerRuntime {
    config: McpServerConfig,
    state: RwLock<ServerRuntimeState>,
}

impl ServerRuntime {
    fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            state: RwLock::new(ServerRuntimeState::default()),
        }
    }

    fn needs_refresh(&self, ttl: Duration) -> bool {
        let guard = match self.state.read() {
            Ok(g) => g,
            Err(_) => return true,
        };
        if guard.health != ServerHealth::Ready {
            return true;
        }
        match guard.last_discovery {
            Some(last) => last.elapsed() >= ttl,
            None => true,
        }
    }

    fn discovered_tools(&self) -> Vec<DiscoveredMcpTool> {
        match self.state.read() {
            Ok(g) => g.tools.clone(),
            Err(_) => Vec::new(),
        }
    }

    fn mark_failure(&self, message: String) {
        if let Ok(mut guard) = self.state.write() {
            guard.health = ServerHealth::Degraded;
            guard.tools.clear();
            guard.last_error = Some(message);
        }
    }

    fn mark_ready(&self, tools: Vec<DiscoveredMcpTool>) {
        if let Ok(mut guard) = self.state.write() {
            guard.health = ServerHealth::Ready;
            guard.tools = tools;
            guard.last_discovery = Some(Instant::now());
            guard.last_error = None;
        }
    }

    async fn refresh(&self, observer: &ObserverHandle) {
        if !self.needs_refresh(Duration::ZERO) {
            return;
        }

        match self.discover_tools().await {
            Ok(discovered) => {
                let count = discovered.len();
                self.mark_ready(discovered);
                observer.log(
                    ObserverCategory::ToolReload,
                    format!(
                        "MCP server '{}' ready ({} allowed tool{})",
                        self.config.name,
                        count,
                        if count == 1 { "" } else { "s" }
                    ),
                );
            }
            Err(e) => {
                self.mark_failure(e.to_string());
                observer.log(
                    ObserverCategory::ToolReload,
                    format!(
                        "MCP server '{}' degraded: {}",
                        self.config.name,
                        truncate_diagnostic(&e.to_string())
                    ),
                );
                tracing::warn!(server = %self.config.name, error = %e, "MCP capability discovery failed");
            }
        }
    }

    async fn discover_tools(&self) -> Result<Vec<DiscoveredMcpTool>> {
        let mut session = McpStdioSession::start(&self.config).await?;
        let result = session.list_tools(&self.config).await;
        session.close().await;
        result
    }

    async fn invoke(&self, remote_tool: &str, args: &Value) -> Result<McpInvocationResult> {
        let mut session = McpStdioSession::start(&self.config).await?;
        let result = session.call_tool(remote_tool, args).await;
        session.close().await;

        match result {
            Ok(invocation) => {
                if let Ok(mut guard) = self.state.write() {
                    guard.health = ServerHealth::Ready;
                    guard.last_error = None;
                }
                Ok(invocation)
            }
            Err(e) => {
                self.mark_failure(e.to_string());
                Err(e)
            }
        }
    }
}

pub struct McpRegistry {
    servers: HashMap<String, Arc<ServerRuntime>>,
    discovery_ttl: Duration,
    observer: ObserverHandle,
}

impl McpRegistry {
    pub fn from_config(config: &McpConfig, observer: ObserverHandle) -> Option<Arc<Self>> {
        if !config.enabled {
            return None;
        }

        let mut servers = HashMap::new();

        for server in &config.servers {
            if !server.enabled {
                continue;
            }

            if servers.contains_key(&server.name) {
                tracing::warn!(
                    server = %server.name,
                    "Duplicate MCP server name found in config; keeping the first entry"
                );
                continue;
            }

            servers.insert(server.name.clone(), Arc::new(ServerRuntime::new(server.clone())));
        }

        if servers.is_empty() {
            tracing::warn!("MCP enabled, but no enabled servers were configured");
            return None;
        }

        Some(Arc::new(Self {
            servers,
            discovery_ttl: Duration::from_secs(config.discovery_ttl_secs.max(1)),
            observer,
        }))
    }

    pub async fn refresh_if_stale(&self) {
        for runtime in self.servers.values() {
            if runtime.needs_refresh(self.discovery_ttl) {
                runtime.refresh(&self.observer).await;
            }
        }
    }

    pub fn discovered_tools(&self) -> Vec<DiscoveredMcpTool> {
        let mut out = Vec::new();
        for runtime in self.servers.values() {
            out.extend(runtime.discovered_tools());
        }
        out
    }

    pub async fn invoke_tool(
        &self,
        server: &str,
        remote_tool: &str,
        args: &Value,
    ) -> Result<McpInvocationResult> {
        let runtime = self
            .servers
            .get(server)
            .ok_or_else(|| AthenaError::Tool(format!("MCP server '{}' is not configured", server)))?
            .clone();

        runtime.invoke(remote_tool, args).await
    }
}

struct McpStdioSession {
    server_name: String,
    timeout: Duration,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpStdioSession {
    async fn start(config: &McpServerConfig) -> Result<Self> {
        if config.transport != McpTransport::Stdio {
            return Err(AthenaError::Tool(format!(
                "MCP server '{}' uses unsupported transport '{}'; currently only 'stdio' is supported",
                config.name,
                config.transport.as_str()
            )));
        }

        let command = config.command.clone().ok_or_else(|| {
            AthenaError::Tool(format!(
                "MCP server '{}' is missing 'command' for stdio transport",
                config.name
            ))
        })?;

        let mut cmd = Command::new(&command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for env_name in &config.env {
            match std::env::var(env_name) {
                Ok(value) => {
                    cmd.env(env_name, value);
                }
                Err(_) => {
                    tracing::warn!(
                        server = %config.name,
                        env = %env_name,
                        "MCP server env var requested but missing from process environment"
                    );
                }
            }
        }

        let mut child = cmd.spawn().map_err(|e| {
            let message = format!(
                "failed to spawn MCP server '{}' command '{}': {}",
                config.name, command, e
            );
            AthenaError::Tool(build_diagnostic(&config.name, McpFailureKind::Connection, &message))
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            AthenaError::Tool(build_diagnostic(
                &config.name,
                McpFailureKind::Connection,
                "spawned process does not expose stdin",
            ))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            AthenaError::Tool(build_diagnostic(
                &config.name,
                McpFailureKind::Connection,
                "spawned process does not expose stdout",
            ))
        })?;

        let mut session = Self {
            server_name: config.name.clone(),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };

        session.initialize().await?;

        Ok(session)
    }

    async fn close(&mut self) {
        let _ = self.stdin.shutdown().await;
        match timeout(
            Duration::from_millis(SHUTDOWN_GRACE_MS),
            self.child.wait(),
        )
        .await
        {
            Ok(_) => {}
            Err(_) => {
                let _ = self.child.start_kill();
            }
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        let init_params = json!({
            "protocolVersion": INIT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION,
            },
        });

        self.request("initialize", init_params).await.map_err(|e| {
            AthenaError::Tool(build_diagnostic(
                &self.server_name,
                classify_failure_kind(&e.to_string(), McpFailureKind::Connection),
                &format!("initialize failed: {}", e),
            ))
        })?;

        self.notify("notifications/initialized", json!({}))
            .await
            .map_err(|e| {
                AthenaError::Tool(build_diagnostic(
                    &self.server_name,
                    classify_failure_kind(&e.to_string(), McpFailureKind::Protocol),
                    &format!("initialized notification failed: {}", e),
                ))
            })
    }

    async fn list_tools(&mut self, config: &McpServerConfig) -> Result<Vec<DiscoveredMcpTool>> {
        let mut cursor: Option<String> = None;
        let mut discovered = Vec::new();

        loop {
            let mut params = serde_json::Map::new();
            if let Some(ref c) = cursor {
                params.insert("cursor".to_string(), Value::String(c.clone()));
            }

            let result = self
                .request("tools/list", Value::Object(params))
                .await
                .map_err(|e| {
                    AthenaError::Tool(build_diagnostic(
                        &self.server_name,
                        classify_failure_kind(&e.to_string(), McpFailureKind::Discovery),
                        &format!("tools/list failed: {}", e),
                    ))
                })?;

            let tools = result
                .get("tools")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    AthenaError::Tool(build_diagnostic(
                        &self.server_name,
                        McpFailureKind::Protocol,
                        "tools/list result missing 'tools' array",
                    ))
                })?;

            for tool in tools {
                let remote_name = tool
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AthenaError::Tool(build_diagnostic(
                            &self.server_name,
                            McpFailureKind::Protocol,
                            "tools/list item missing 'name'",
                        ))
                    })?
                    .to_string();

                if !tool_allowed(config, &remote_name) {
                    continue;
                }

                let description = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Remote MCP capability")
                    .to_string();

                let schema = tool
                    .get("inputSchema")
                    .or_else(|| tool.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(default_parameter_schema);

                discovered.push(DiscoveredMcpTool {
                    server: self.server_name.clone(),
                    remote_name: remote_name.clone(),
                    namespaced_name: namespaced_tool_name(&self.server_name, &remote_name),
                    description,
                    input_schema: normalize_parameter_schema(schema),
                    requires_confirmation: config.requires_confirmation,
                });
            }

            cursor = result
                .get("nextCursor")
                .or_else(|| result.get("next_cursor"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if cursor.is_none() {
                break;
            }
        }

        Ok(discovered)
    }

    async fn call_tool(&mut self, remote_tool: &str, args: &Value) -> Result<McpInvocationResult> {
        let params = json!({
            "name": remote_tool,
            "arguments": args.clone(),
        });

        let result = self.request("tools/call", params).await.map_err(|e| {
            AthenaError::Tool(build_diagnostic(
                &self.server_name,
                classify_failure_kind(&e.to_string(), McpFailureKind::Invocation),
                &format!("tools/call failed for '{}': {}", remote_tool, e),
            ))
        })?;

        let success = !result
            .get("isError")
            .or_else(|| result.get("is_error"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut output = extract_content_text(&result);
        if output.is_empty() {
            output = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
        }

        Ok(McpInvocationResult { success, output })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);

        let message = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });

        self.write_message(&message).await?;

        loop {
            let response = self.read_message().await?;

            if !response_matches_id(&response, request_id) {
                continue;
            }

            if let Some(err) = response.get("error") {
                let msg = format_jsonrpc_error(err);
                return Err(AthenaError::Tool(msg));
            }

            if let Some(result) = response.get("result") {
                return Ok(result.clone());
            }

            return Err(AthenaError::Tool(
                "MCP protocol error: response missing both 'result' and 'error'".to_string(),
            ));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&message).await
    }

    async fn write_message(&mut self, value: &Value) -> Result<()> {
        let payload = serde_json::to_vec(value)
            .map_err(|e| AthenaError::Tool(format!("MCP serialization error: {}", e)))?;
        let header = format!("Content-Length: {}\r\n\r\n", payload.len());

        timeout(self.timeout, self.stdin.write_all(header.as_bytes()))
            .await
            .map_err(|_| AthenaError::Tool("MCP write timed out".to_string()))
            .and_then(|r| {
                r.map_err(|e| AthenaError::Tool(format!("MCP write error: {}", e)))
            })?;

        timeout(self.timeout, self.stdin.write_all(&payload))
            .await
            .map_err(|_| AthenaError::Tool("MCP write timed out".to_string()))
            .and_then(|r| {
                r.map_err(|e| AthenaError::Tool(format!("MCP write error: {}", e)))
            })?;

        timeout(self.timeout, self.stdin.flush())
            .await
            .map_err(|_| AthenaError::Tool("MCP flush timed out".to_string()))
            .and_then(|r| {
                r.map_err(|e| AthenaError::Tool(format!("MCP flush error: {}", e)))
            })?;

        Ok(())
    }

    async fn read_message(&mut self) -> Result<Value> {
        let mut first_line = String::new();
        let first_read = timeout(self.timeout, self.stdout.read_line(&mut first_line))
            .await
            .map_err(|_| AthenaError::Tool("MCP read timed out".to_string()))?
            .map_err(|e| AthenaError::Tool(format!("MCP read error: {}", e)))?;

        if first_read == 0 {
            return Err(AthenaError::Tool("MCP stream closed by server".to_string()));
        }

        if first_line.trim_start().starts_with('{') {
            return serde_json::from_str(first_line.trim()).map_err(|e| {
                AthenaError::Tool(format!("MCP protocol error: invalid JSON frame: {}", e))
            });
        }

        let mut content_length = parse_content_length(&first_line);

        loop {
            let mut header_line = String::new();
            let read = timeout(self.timeout, self.stdout.read_line(&mut header_line))
                .await
                .map_err(|_| AthenaError::Tool("MCP read timed out".to_string()))?
                .map_err(|e| AthenaError::Tool(format!("MCP read error: {}", e)))?;

            if read == 0 {
                return Err(AthenaError::Tool(
                    "MCP protocol error: EOF while reading headers".to_string(),
                ));
            }

            if header_line == "\r\n" || header_line == "\n" {
                break;
            }

            if content_length.is_none() {
                content_length = parse_content_length(&header_line);
            }
        }

        let length = content_length.ok_or_else(|| {
            AthenaError::Tool("MCP protocol error: missing Content-Length header".to_string())
        })?;

        let mut payload = vec![0_u8; length];
        timeout(self.timeout, self.stdout.read_exact(&mut payload))
            .await
            .map_err(|_| AthenaError::Tool("MCP read timed out".to_string()))?
            .map_err(|e| AthenaError::Tool(format!("MCP read error: {}", e)))?;

        serde_json::from_slice(&payload)
            .map_err(|e| AthenaError::Tool(format!("MCP protocol error: invalid JSON: {}", e)))
    }
}

fn tool_allowed(config: &McpServerConfig, remote_tool: &str) -> bool {
    if config.allowed_tools.is_empty() {
        return false;
    }

    let namespaced = namespaced_tool_name(&config.name, remote_tool);
    config
        .allowed_tools
        .iter()
        .any(|allowed| allowed == "*" || allowed == remote_tool || allowed == &namespaced)
}

fn namespaced_tool_name(server: &str, remote_tool: &str) -> String {
    format!("mcp:{}:{}", server, remote_tool)
}

fn normalize_parameter_schema(schema: Value) -> Value {
    if schema
        .get("type")
        .and_then(|v| v.as_str())
        .map(|v| v == "object")
        .unwrap_or(false)
    {
        schema
    } else {
        default_parameter_schema()
    }
}

fn default_parameter_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": true,
    })
}

fn parse_content_length(line: &str) -> Option<usize> {
    let (key, value) = line.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    value.trim().parse::<usize>().ok()
}

fn response_matches_id(response: &Value, request_id: u64) -> bool {
    let id = match response.get("id") {
        Some(id) => id,
        None => return false,
    };

    if id.as_u64() == Some(request_id) {
        return true;
    }

    match id.as_str() {
        Some(raw) => raw.parse::<u64>().ok() == Some(request_id),
        None => false,
    }
}

fn format_jsonrpc_error(error: &Value) -> String {
    let code = error.get("code").map(|v| v.to_string()).unwrap_or_default();
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown JSON-RPC error");

    if code.is_empty() {
        format!("MCP error: {}", message)
    } else {
        format!("MCP error {}: {}", code, message)
    }
}

fn extract_content_text(result: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for item in content {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        if let Some(text) = result
            .get("structuredContent")
            .or_else(|| result.get("structured_content"))
        {
            let serialized = serde_json::to_string_pretty(text).unwrap_or_else(|_| text.to_string());
            if !serialized.trim().is_empty() {
                parts.push(serialized);
            }
        }
    }

    parts.join("\n")
}

fn classify_failure_kind(message: &str, default_kind: McpFailureKind) -> McpFailureKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return McpFailureKind::Timeout;
    }
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authentication")
        || lower.contains("api key")
        || lower.contains("token")
    {
        return McpFailureKind::Auth;
    }
    if lower.contains("protocol") || lower.contains("json") || lower.contains("content-length") {
        return McpFailureKind::Protocol;
    }
    default_kind
}

fn build_diagnostic(server: &str, kind: McpFailureKind, detail: &str) -> String {
    let hint = match kind {
        McpFailureKind::Connection => {
            "Verify `mcp.servers[].command` and `args`, and ensure the MCP server binary is installed."
        }
        McpFailureKind::Auth => {
            "Check auth environment variables listed in `mcp.servers[].env` and credentials for this server."
        }
        McpFailureKind::Discovery => {
            "Confirm the server supports `tools/list` and that allowed tool names are correct."
        }
        McpFailureKind::Invocation => {
            "Check tool arguments against the server's `inputSchema` and review server logs."
        }
        McpFailureKind::Timeout => {
            "Increase `mcp.servers[].timeout_secs` or verify the server is responsive."
        }
        McpFailureKind::Protocol => {
            "Confirm protocol compatibility with MCP JSON-RPC framing and schema fields."
        }
    };

    format!(
        "MCP {:?} failure for server '{}': {}. Hint: {}",
        kind, server, detail, hint
    )
}

fn truncate_diagnostic(input: &str) -> String {
    const MAX: usize = 160;
    if input.len() <= MAX {
        input.to_string()
    } else {
        let boundary = input.floor_char_boundary(MAX);
        format!("{}...", &input[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpConfig, McpServerConfig, McpTransport};

    fn server_config(allowed_tools: Vec<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "demo".to_string(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            env: vec![],
            timeout_secs: 10,
            reconnect_delay_secs: 5,
            requires_confirmation: true,
            allowed_tools: allowed_tools.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn tool_allowlist_is_default_deny() {
        let cfg = server_config(vec![]);
        assert!(!tool_allowed(&cfg, "search"));
    }

    #[test]
    fn tool_allowlist_accepts_wildcard_and_namespaced() {
        let cfg = server_config(vec!["mcp:demo:search", "*", "ignored"]);
        assert!(tool_allowed(&cfg, "search"));
    }

    #[test]
    fn namespaced_tool_format_is_stable() {
        assert_eq!(namespaced_tool_name("linear", "query"), "mcp:linear:query");
    }

    #[test]
    fn content_length_parser_handles_case_and_spaces() {
        assert_eq!(parse_content_length("Content-Length: 17\r\n"), Some(17));
        assert_eq!(parse_content_length("content-length: 9\n"), Some(9));
        assert_eq!(parse_content_length("X: 9\n"), None);
    }

    #[test]
    fn extract_content_text_prefers_text_blocks() {
        let payload = json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ]
        });

        assert_eq!(extract_content_text(&payload), "first\nsecond");
    }

    #[test]
    fn classify_failure_prefers_timeout_and_auth_signals() {
        assert_eq!(
            classify_failure_kind("Request timed out", McpFailureKind::Invocation),
            McpFailureKind::Timeout
        );
        assert_eq!(
            classify_failure_kind("HTTP 401 unauthorized", McpFailureKind::Invocation),
            McpFailureKind::Auth
        );
    }

    #[test]
    fn registry_not_created_when_disabled() {
        let cfg = McpConfig {
            enabled: false,
            discovery_ttl_secs: 60,
            servers: vec![server_config(vec!["search"])],
        };
        let observer = ObserverHandle::new(4);
        assert!(McpRegistry::from_config(&cfg, observer).is_none());
    }
}
