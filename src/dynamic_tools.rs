use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docker::DockerSession;
use crate::error::{SparksError, Result};
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::tools::{Tool, ToolResult};

/// Where a dynamic tool's command runs.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    #[default]
    Docker,
    Host,
}

/// The data type of a tool parameter.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParameterType {
    String,
    Number,
    Boolean,
}

/// Defines a single parameter for a dynamic tool.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ParameterDefinition {
    /// The name of the parameter.
    pub name: std::string::String,
    /// A description of what the parameter is for.
    pub description: std::string::String,
    /// The data type of the parameter.
    #[serde(rename = "type")]
    pub param_type: ParameterType,
    /// Whether the parameter is required.
    #[serde(default)]
    pub required: bool,
    /// A default value for the parameter if it's not provided.
    #[serde(default)]
    pub default: Option<Value>,
}

/// YAML-based tool definition (Tool Description Language)
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DynamicToolDefinition {
    /// Tool name (used as the identifier in tool calls)
    pub name: String,
    /// Human-readable description shown to the LLM
    pub description: String,
    /// A list of parameters the tool accepts.
    #[serde(default)]
    pub parameters: Vec<ParameterDefinition>,
    /// Whether this tool requires user confirmation before execution
    #[serde(default)]
    pub needs_confirmation: bool,
    /// Shell command template. Use `{{param_name}}` for parameter substitution.
    pub command: String,
    /// Where to run: docker (default) or host
    #[serde(default)]
    pub execution: ExecutionMode,
    /// For host tools: first word of the rendered command must be in this list
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    /// For host tools: command must not contain any of these substrings
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
    /// Command timeout in seconds (default: 120 for host, none/docker-default for docker)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A tool loaded at runtime from a YAML definition file.
pub struct DynamicTool {
    def: DynamicToolDefinition,
    /// Working directory for host-executed tools
    host_workspace: Option<String>,
}

impl DynamicTool {
    pub fn new(def: DynamicToolDefinition, host_workspace: Option<String>) -> Self {
        Self {
            def,
            host_workspace,
        }
    }

    /// Tool name accessor
    pub fn tool_name(&self) -> &str {
        &self.def.name
    }

    /// Whether this tool requires user confirmation
    pub fn requires_confirmation(&self) -> bool {
        self.def.needs_confirmation || self.def.execution == ExecutionMode::Host
    }

    /// Render command template and validate against security rules.
    /// Returns the rendered command or an error.
    pub fn validate_and_render(&self, params: &Value) -> std::result::Result<String, String> {
        let cmd = self.render_command(params);
        // Check allowed_commands
        if !self.def.allowed_commands.is_empty() {
            let first_word = cmd.split_whitespace().next().unwrap_or("");
            if !self.def.allowed_commands.iter().any(|a| a == first_word) {
                return Err(format!(
                    "Command '{}' not in allowed list: {:?}",
                    first_word, self.def.allowed_commands
                ));
            }
        }
        // Check blocked_patterns
        for pattern in &self.def.blocked_patterns {
            if cmd.contains(pattern.as_str()) {
                return Err(format!("Blocked dangerous pattern: '{}'", pattern));
            }
        }
        Ok(cmd)
    }

    /// Brief description for classifier prompt, including parameter details
    pub fn classifier_description(&self) -> String {
        if self.def.parameters.is_empty() {
            return format!("{} — {}", self.def.name, self.def.description);
        }
        let param_details: Vec<String> = self
            .def
            .parameters
            .iter()
            .map(|p| format!("{}: {}", p.name, p.description))
            .collect();
        format!(
            "{} — {}\n    Parameters: {}",
            self.def.name,
            self.def.description,
            param_details.join("; ")
        )
    }

    /// Render the command template by substituting `{{key}}` with values from params.
    fn render_command(&self, params: &Value) -> String {
        let mut cmd = self.def.command.clone();
        if let Some(obj) = params.as_object() {
            for (key, val) in obj {
                let placeholder = format!("{{{{{}}}}}", key);
                let replacement = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cmd = cmd.replace(&placeholder, &replacement);
            }
        }
        cmd
    }
}

const DYNAMIC_OUTPUT_LEN: usize = 4000;

/// Truncate output to prevent context bloat
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...\n[truncated, {} total chars]", &s[..max], s.len())
    }
}

#[async_trait]
impl Tool for DynamicTool {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn parameter_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for p in &self.def.parameters {
            let type_str = match p.param_type {
                ParameterType::String => "string",
                ParameterType::Number => "number",
                ParameterType::Boolean => "boolean",
            };
            let mut prop = serde_json::json!({
                "type": type_str,
                "description": p.description,
            });
            if let Some(ref default) = p.default {
                prop["default"] = default.clone();
            }
            properties.insert(p.name.clone(), prop);
            if p.required {
                required.push(Value::String(p.name.clone()));
            }
        }

        let mut schema = serde_json::json!({
            "type": "object",
            "properties": properties,
        });
        if !required.is_empty() {
            schema["required"] = Value::Array(required);
        }
        schema
    }

    fn description(&self) -> String {
        if self.def.parameters.is_empty() {
            return self.def.description.clone();
        }

        let mut desc = self.def.description.clone();
        desc.push_str("\nParameters:");
        for p in &self.def.parameters {
            let type_str = match p.param_type {
                ParameterType::String => "string",
                ParameterType::Number => "number",
                ParameterType::Boolean => "boolean",
            };
            let req = if p.required { "required" } else { "optional" };
            desc.push_str(&format!(
                "\n  - {} ({}{}): {}",
                p.name,
                type_str,
                if p.required {
                    format!(", {}", req)
                } else {
                    match &p.default {
                        Some(v) => format!(", default={}", v),
                        None => format!(", {}", req),
                    }
                },
                p.description,
            ));
        }
        desc
    }

    fn needs_confirmation(&self) -> bool {
        // Host tools ALWAYS need confirmation regardless of YAML setting
        self.def.needs_confirmation || self.def.execution == ExecutionMode::Host
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let cmd = self.render_command(params);

        match self.def.execution {
            ExecutionMode::Docker => {
                let output = session.exec(&cmd).await?;
                Ok(ToolResult {
                    success: true,
                    output: truncate(&output, DYNAMIC_OUTPUT_LEN),
                })
            }
            ExecutionMode::Host => {
                // Security: check allowed_commands
                if !self.def.allowed_commands.is_empty() {
                    let first_word = cmd.split_whitespace().next().unwrap_or("");
                    if !self.def.allowed_commands.iter().any(|a| a == first_word) {
                        return Ok(ToolResult {
                            success: false,
                            output: format!(
                                "Command '{}' not in allowed list: {:?}",
                                first_word, self.def.allowed_commands
                            ),
                        });
                    }
                }
                // Security: check blocked_patterns
                for pattern in &self.def.blocked_patterns {
                    if cmd.contains(pattern.as_str()) {
                        return Ok(ToolResult {
                            success: false,
                            output: format!("Blocked dangerous pattern: '{}'", pattern),
                        });
                    }
                }
                self.execute_host(&cmd).await
            }
        }
    }
}

impl DynamicTool {
    /// Run a command on the host via tokio::process::Command.
    pub async fn execute_host(&self, cmd: &str) -> Result<ToolResult> {
        use tokio::process::Command;

        let workspace = self.host_workspace.as_deref().unwrap_or(".");
        let timeout = self.def.timeout_secs.unwrap_or(120);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            Command::new("sh")
                .args(["-c", cmd])
                .current_dir(workspace)
                .env("TERM", "dumb")
                .env("GIT_TERMINAL_PROMPT", "0")
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
                    output: truncate(&combined, DYNAMIC_OUTPUT_LEN),
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: format!("{}: command failed — {}", self.def.name, e),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: format!("{}: timed out after {}s", self.def.name, timeout),
            }),
        }
    }
}

/// Scan a directory for `.yml`/`.yaml` tool definition files and return parsed tools.
/// `host_workspace` is passed to host-executed tools as their working directory.
pub fn discover(path: &Path, host_workspace: &str) -> Result<Vec<Box<dyn Tool>>> {
    if !path.is_dir() {
        tracing::debug!("Dynamic tools path does not exist: {}", path.display());
        return Ok(vec![]);
    }

    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    let entries = std::fs::read_dir(path).map_err(|e| {
        SparksError::Config(format!(
            "Failed to read dynamic tools dir {}: {}",
            path.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to read dir entry in {}: {}", path.display(), e);
                continue;
            }
        };

        let file_path = entry.path();
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yml" && ext != "yaml" {
            continue;
        }

        let contents = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", file_path.display(), e);
                continue;
            }
        };

        let def: DynamicToolDefinition = match serde_yaml::from_str(&contents) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", file_path.display(), e);
                continue;
            }
        };

        let ws = match def.execution {
            ExecutionMode::Host => Some(host_workspace.to_string()),
            ExecutionMode::Docker => None,
        };
        tracing::info!(
            "Loaded dynamic tool '{}' ({:?}) from {}",
            def.name,
            def.execution,
            file_path.display()
        );
        tools.push(Box::new(DynamicTool::new(def, ws)));
    }

    Ok(tools)
}

/// Like `discover()` but returns concrete `DynamicTool` instances filtered to `ExecutionMode::Host` only.
/// Used by the Manager for the direct execution fast path.
pub fn discover_host(path: &Path, host_workspace: &str) -> Result<Vec<DynamicTool>> {
    if !path.is_dir() {
        tracing::debug!("Dynamic tools path does not exist: {}", path.display());
        return Ok(vec![]);
    }

    let mut tools = Vec::new();

    let entries = std::fs::read_dir(path).map_err(|e| {
        SparksError::Config(format!(
            "Failed to read dynamic tools dir {}: {}",
            path.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to read dir entry in {}: {}", path.display(), e);
                continue;
            }
        };

        let file_path = entry.path();
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yml" && ext != "yaml" {
            continue;
        }

        let contents = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", file_path.display(), e);
                continue;
            }
        };

        let def: DynamicToolDefinition = match serde_yaml::from_str(&contents) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", file_path.display(), e);
                continue;
            }
        };

        if def.execution != ExecutionMode::Host {
            continue;
        }

        tracing::info!(
            "Loaded host tool '{}' for direct path from {}",
            def.name,
            file_path.display()
        );
        tools.push(DynamicTool::new(def, Some(host_workspace.to_string())));
    }

    Ok(tools)
}

/// Spawn a background task that watches the dynamic tools directory for changes
/// and rebuilds the direct_tools map when files are created, modified, or removed.
/// Uses `notify` crate (kqueue on macOS, inotify on Linux).
pub fn spawn_hot_reload(
    path: PathBuf,
    host_workspace: String,
    direct_tools: Arc<tokio::sync::RwLock<HashMap<String, DynamicTool>>>,
    observer: ObserverHandle,
) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    // Ensure the directory exists before watching
    if !path.is_dir() {
        if let Err(e) = std::fs::create_dir_all(&path) {
            tracing::warn!("Failed to create dynamic tools dir for hot-reload: {}", e);
            return;
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);

    // Start the file watcher on a blocking thread
    let watch_path = path.clone();
    std::thread::spawn(move || {
        let tx = tx;
        let mut watcher = match notify::recommended_watcher(
            move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                            let _ = tx.blocking_send(());
                        }
                        _ => {}
                    }
                }
            },
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("Failed to create file watcher for hot-reload: {}", e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&watch_path, RecursiveMode::NonRecursive) {
            tracing::warn!("Failed to watch {}: {}", watch_path.display(), e);
            return;
        }

        tracing::info!("Hot-reload watcher active on {}", watch_path.display());

        // Keep watcher alive until the thread is terminated (process exit)
        loop {
            std::thread::park();
        }
    });

    // Debounce + reload loop on async runtime
    tokio::spawn(async move {
        loop {
            // Wait for first notification
            if rx.recv().await.is_none() {
                break; // Channel closed
            }

            // Debounce: drain any rapid-fire events for 500ms
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            while rx.try_recv().is_ok() {} // drain

            // Rebuild the tool map
            match discover_host(&path, &host_workspace) {
                Ok(tools) => {
                    let count = tools.len();
                    let names: Vec<String> =
                        tools.iter().map(|t| t.tool_name().to_string()).collect();
                    let new_map: HashMap<String, DynamicTool> = tools
                        .into_iter()
                        .map(|t| (t.tool_name().to_string(), t))
                        .collect();

                    let mut lock = direct_tools.write().await;
                    *lock = new_map;
                    drop(lock);

                    observer.emit(crate::observer::ObserverEvent::new(
                        ObserverCategory::ToolReload,
                        format!("Reloaded {} host tool(s): {:?}", count, names),
                    ));
                    tracing::info!("Hot-reloaded {} host tool(s): {:?}", count, names);
                }
                Err(e) => {
                    tracing::warn!("Hot-reload failed: {}", e);
                    observer.emit(crate::observer::ObserverEvent::new(
                        ObserverCategory::ToolReload,
                        format!("Hot-reload error: {}", e),
                    ));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_def(name: &str, command: &str) -> DynamicToolDefinition {
        DynamicToolDefinition {
            name: name.into(),
            description: format!("test tool {}", name),
            parameters: vec![],
            needs_confirmation: false,
            command: command.into(),
            execution: ExecutionMode::default(),
            allowed_commands: vec![],
            blocked_patterns: vec![],
            timeout_secs: None,
        }
    }

    fn make_host_def(name: &str, command: &str) -> DynamicToolDefinition {
        DynamicToolDefinition {
            execution: ExecutionMode::Host,
            allowed_commands: vec!["git".into()],
            blocked_patterns: vec!["push --force".into(), "reset --hard".into()],
            timeout_secs: Some(120),
            ..make_def(name, command)
        }
    }

    #[test]
    fn test_render_command_substitution() {
        let def = make_def("test", "echo {{message}} > {{file}}");
        let tool = DynamicTool::new(def, None);

        let params = serde_json::json!({
            "message": "hello world",
            "file": "/tmp/out.txt"
        });

        assert_eq!(
            tool.render_command(&params),
            "echo hello world > /tmp/out.txt"
        );
    }

    #[test]
    fn test_render_command_no_params() {
        let tool = DynamicTool::new(make_def("test", "ls -la"), None);
        let params = serde_json::json!({});
        assert_eq!(tool.render_command(&params), "ls -la");
    }

    #[test]
    fn test_render_command_missing_param() {
        let tool = DynamicTool::new(make_def("test", "echo {{name}} {{missing}}"), None);
        let params = serde_json::json!({"name": "alice"});
        // Missing placeholders are left as-is
        assert_eq!(tool.render_command(&params), "echo alice {{missing}}");
    }

    #[test]
    fn test_render_command_non_string_param() {
        let tool = DynamicTool::new(make_def("test", "echo {{count}}"), None);
        let params = serde_json::json!({"count": 42});
        assert_eq!(tool.render_command(&params), "echo 42");
    }

    #[test]
    fn test_tool_metadata() {
        let mut def = make_def("my_tool", "echo hi");
        def.description = "Does something cool".into();
        def.needs_confirmation = true;
        let tool = DynamicTool::new(def, None);
        assert_eq!(tool.name(), "my_tool");
        assert_eq!(tool.description(), "Does something cool");
        assert!(tool.needs_confirmation());
    }

    #[test]
    fn test_discover_nonexistent_dir() {
        let result = discover(Path::new("/nonexistent/path/for/testing"), ".");
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_discover_empty_dir() {
        let dir = std::env::temp_dir().join("sparks_test_dynamic_tools_empty");
        let _ = std::fs::create_dir_all(&dir);
        let result = discover(&dir, ".");
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_loads_yaml() {
        let dir = std::env::temp_dir().join("sparks_test_dynamic_tools_load");
        let _ = std::fs::create_dir_all(&dir);

        let yaml = r#"
name: hello
description: "Say hello: {\"tool\": \"hello\", \"params\": {\"name\": \"...\"}}"
needs_confirmation: false
command: "echo Hello {{name}}"
"#;
        std::fs::write(dir.join("hello.yml"), yaml).unwrap();

        let result = discover(&dir, ".").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), "hello");
        assert!(!result[0].needs_confirmation());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_skips_non_yaml() {
        let dir = std::env::temp_dir().join("sparks_test_dynamic_tools_skip");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("readme.txt"), "not a tool").unwrap();
        std::fs::write(dir.join("tool.json"), "{}").unwrap();

        let result = discover(&dir, ".").unwrap();
        assert!(result.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_skips_invalid_yaml() {
        let dir = std::env::temp_dir().join("sparks_test_dynamic_tools_invalid");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("bad.yml"), "this: is\nnot: valid\ntool: def").unwrap();

        let result = discover(&dir, ".").unwrap();
        assert!(result.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ExecutionMode deserialization ────────────────────────────────

    #[test]
    fn test_execution_mode_default_is_docker() {
        let yaml = r#"
name: test
description: "test"
command: "echo hi"
"#;
        let def: DynamicToolDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(def.execution, ExecutionMode::Docker);
        assert!(def.allowed_commands.is_empty());
        assert!(def.blocked_patterns.is_empty());
        assert!(def.timeout_secs.is_none());
    }

    #[test]
    fn test_execution_mode_host_deserialization() {
        let yaml = r#"
name: git
description: "git tool"
execution: host
allowed_commands: ["git"]
blocked_patterns: ["push --force", "reset --hard"]
timeout_secs: 60
command: "git {{subcommand}}"
"#;
        let def: DynamicToolDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(def.execution, ExecutionMode::Host);
        assert_eq!(def.allowed_commands, vec!["git"]);
        assert_eq!(def.blocked_patterns, vec!["push --force", "reset --hard"]);
        assert_eq!(def.timeout_secs, Some(60));
    }

    // ── Security: allowed_commands ──────────────────────────────────

    /// Helper: simulates the allowed_commands check from execute()
    fn check_allowed(tool: &DynamicTool, cmd: &str) -> bool {
        if tool.def.allowed_commands.is_empty() {
            return true;
        }
        let first_word = cmd.split_whitespace().next().unwrap_or("");
        tool.def.allowed_commands.iter().any(|a| a == first_word)
    }

    /// Helper: simulates the blocked_patterns check from execute()
    fn check_blocked(tool: &DynamicTool, cmd: &str) -> Option<String> {
        for pattern in &tool.def.blocked_patterns {
            if cmd.contains(pattern.as_str()) {
                return Some(pattern.clone());
            }
        }
        None
    }

    #[test]
    fn test_host_allowed_commands_rejection() {
        let def = make_host_def("git", "rm -rf /");
        let tool = DynamicTool::new(def, Some(".".into()));
        assert!(
            !check_allowed(&tool, "rm -rf /"),
            "rm should not be in allowed list"
        );
    }

    #[test]
    fn test_host_allowed_commands_pass() {
        let def = make_host_def("git", "git {{subcommand}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let cmd = tool.render_command(&serde_json::json!({"subcommand": "status"}));
        assert!(check_allowed(&tool, &cmd), "git should be in allowed list");
    }

    // ── Security: blocked_patterns ──────────────────────────────────

    #[test]
    fn test_host_blocked_patterns_rejection() {
        let def = make_host_def("git", "git {{subcommand}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let cmd =
            tool.render_command(&serde_json::json!({"subcommand": "push --force origin main"}));
        assert!(
            check_blocked(&tool, &cmd).is_some(),
            "Should block 'push --force'"
        );
    }

    #[test]
    fn test_host_blocked_patterns_pass() {
        let def = make_host_def("git", "git {{subcommand}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let cmd = tool.render_command(&serde_json::json!({"subcommand": "push origin main"}));
        assert!(
            check_blocked(&tool, &cmd).is_none(),
            "Normal push should not be blocked"
        );
    }

    // ── Host tool forced confirmation ───────────────────────────────

    #[test]
    fn test_host_tool_forced_confirmation() {
        // Even with needs_confirmation=false in YAML, host tools must confirm
        let mut def = make_host_def("git", "git status");
        def.needs_confirmation = false;
        let tool = DynamicTool::new(def, Some(".".into()));
        assert!(
            tool.needs_confirmation(),
            "Host tools must always require confirmation"
        );
    }

    #[test]
    fn test_docker_tool_no_forced_confirmation() {
        let def = make_def("echo", "echo hi");
        let tool = DynamicTool::new(def, None);
        assert!(
            !tool.needs_confirmation(),
            "Docker tools respect YAML setting"
        );
    }

    // ── Discover host tools ─────────────────────────────────────────

    #[test]
    fn test_discover_host_tool_gets_workspace() {
        let dir = std::env::temp_dir().join("sparks_test_dynamic_tools_host");
        let _ = std::fs::create_dir_all(&dir);

        let yaml = r#"
name: git
description: "git tool"
execution: host
allowed_commands: ["git"]
command: "git {{subcommand}}"
"#;
        std::fs::write(dir.join("git.yml"), yaml).unwrap();

        let result = discover(&dir, "/my/workspace").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), "git");
        // Host tools should always require confirmation
        assert!(result[0].needs_confirmation());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── validate_and_render ─────────────────────────────────────────

    #[test]
    fn test_validate_and_render_ok() {
        let def = make_host_def("git", "git {{subcommand}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let result = tool.validate_and_render(&serde_json::json!({"subcommand": "status"}));
        assert_eq!(result, Ok("git status".to_string()));
    }

    #[test]
    fn test_validate_and_render_blocked() {
        let def = make_host_def("git", "git {{subcommand}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let result = tool.validate_and_render(&serde_json::json!({"subcommand": "push --force"}));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Blocked"));
    }

    #[test]
    fn test_validate_and_render_disallowed_command() {
        let def = make_host_def("git", "{{cmd}}");
        let tool = DynamicTool::new(def, Some(".".into()));
        let result = tool.validate_and_render(&serde_json::json!({"cmd": "rm -rf /"}));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not in allowed list"));
    }

    // ── classifier_description ──────────────────────────────────────

    #[test]
    fn test_classifier_description() {
        let mut def = make_host_def("git", "git {{subcommand}}");
        def.parameters = vec![ParameterDefinition {
            name: "subcommand".into(),
            description: "Git subcommand".into(),
            param_type: ParameterType::String,
            required: true,
            default: None,
        }];
        let tool = DynamicTool::new(def, Some(".".into()));
        let desc = tool.classifier_description();
        assert!(desc.contains("git"));
        assert!(desc.contains("subcommand"));
    }

    // ── discover_host ───────────────────────────────────────────────

    #[test]
    fn test_discover_host_filters_docker_tools() {
        let dir = std::env::temp_dir().join("sparks_test_discover_host");
        let _ = std::fs::create_dir_all(&dir);

        // Host tool
        let host_yaml = r#"
name: git
description: "git tool"
execution: host
allowed_commands: ["git"]
command: "git {{subcommand}}"
"#;
        std::fs::write(dir.join("git.yml"), host_yaml).unwrap();

        // Docker tool (should be filtered out)
        let docker_yaml = r#"
name: echo
description: "echo tool"
command: "echo hi"
"#;
        std::fs::write(dir.join("echo.yml"), docker_yaml).unwrap();

        let result = discover_host(&dir, ".").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tool_name(), "git");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
