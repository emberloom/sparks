use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docker::DockerSession;
use crate::error::{AthenaError, Result};
use crate::tools::{Tool, ToolResult};

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
}

/// A tool loaded at runtime from a YAML definition file.
pub struct DynamicTool {
    def: DynamicToolDefinition,
}

impl DynamicTool {
    fn new(def: DynamicToolDefinition) -> Self {
        Self { def }
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
            desc.push_str(&format!("\n  - {} ({}{}): {}",
                p.name,
                type_str,
                if p.required { format!(", {}", req) } else {
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
        self.def.needs_confirmation
    }

    async fn execute(&self, session: &DockerSession, params: &Value) -> Result<ToolResult> {
        let cmd = self.render_command(params);
        let output = session.exec(&cmd).await?;
        Ok(ToolResult {
            success: true,
            output: truncate(&output, DYNAMIC_OUTPUT_LEN),
        })
    }
}

/// Scan a directory for `.yml`/`.yaml` tool definition files and return parsed tools.
pub fn discover(path: &Path) -> Result<Vec<Box<dyn Tool>>> {
    if !path.is_dir() {
        tracing::debug!("Dynamic tools path does not exist: {}", path.display());
        return Ok(vec![]);
    }

    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    let entries = std::fs::read_dir(path)
        .map_err(|e| AthenaError::Config(format!("Failed to read dynamic tools dir {}: {}", path.display(), e)))?;

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

        tracing::info!("Loaded dynamic tool '{}' from {}", def.name, file_path.display());
        tools.push(Box::new(DynamicTool::new(def)));
    }

    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_command_substitution() {
        let def = DynamicToolDefinition {
            name: "test".into(),
            description: "test tool".into(),
            parameters: vec![],
            needs_confirmation: false,
            command: "echo {{message}} > {{file}}".into(),
        };
        let tool = DynamicTool::new(def);

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
        let def = DynamicToolDefinition {
            name: "test".into(),
            description: "test tool".into(),
            parameters: vec![],
            needs_confirmation: false,
            command: "ls -la".into(),
        };
        let tool = DynamicTool::new(def);
        let params = serde_json::json!({});
        assert_eq!(tool.render_command(&params), "ls -la");
    }

    #[test]
    fn test_render_command_missing_param() {
        let def = DynamicToolDefinition {
            name: "test".into(),
            description: "test tool".into(),
            parameters: vec![],
            needs_confirmation: false,
            command: "echo {{name}} {{missing}}".into(),
        };
        let tool = DynamicTool::new(def);
        let params = serde_json::json!({"name": "alice"});
        // Missing placeholders are left as-is
        assert_eq!(tool.render_command(&params), "echo alice {{missing}}");
    }

    #[test]
    fn test_render_command_non_string_param() {
        let def = DynamicToolDefinition {
            name: "test".into(),
            description: "test tool".into(),
            parameters: vec![],
            needs_confirmation: false,
            command: "echo {{count}}".into(),
        };
        let tool = DynamicTool::new(def);
        let params = serde_json::json!({"count": 42});
        assert_eq!(tool.render_command(&params), "echo 42");
    }

    #[test]
    fn test_tool_metadata() {
        let def = DynamicToolDefinition {
            name: "my_tool".into(),
            description: "Does something cool".into(),
            parameters: vec![],
            needs_confirmation: true,
            command: "echo hi".into(),
        };
        let tool = DynamicTool::new(def);
        assert_eq!(tool.name(), "my_tool");
        assert_eq!(tool.description(), "Does something cool");
        assert!(tool.needs_confirmation());
    }

    #[test]
    fn test_discover_nonexistent_dir() {
        let result = discover(Path::new("/nonexistent/path/for/testing"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_discover_empty_dir() {
        let dir = std::env::temp_dir().join("athena_test_dynamic_tools_empty");
        let _ = std::fs::create_dir_all(&dir);
        let result = discover(&dir);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_loads_yaml() {
        let dir = std::env::temp_dir().join("athena_test_dynamic_tools_load");
        let _ = std::fs::create_dir_all(&dir);

        let yaml = r#"
name: hello
description: "Say hello: {\"tool\": \"hello\", \"params\": {\"name\": \"...\"}}"
needs_confirmation: false
command: "echo Hello {{name}}"
"#;
        std::fs::write(dir.join("hello.yml"), yaml).unwrap();

        let result = discover(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), "hello");
        assert!(!result[0].needs_confirmation());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_skips_non_yaml() {
        let dir = std::env::temp_dir().join("athena_test_dynamic_tools_skip");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("readme.txt"), "not a tool").unwrap();
        std::fs::write(dir.join("tool.json"), "{}").unwrap();

        let result = discover(&dir).unwrap();
        assert!(result.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_discover_skips_invalid_yaml() {
        let dir = std::env::temp_dir().join("athena_test_dynamic_tools_invalid");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("bad.yml"), "this: is\nnot: valid\ntool: def").unwrap();

        let result = discover(&dir).unwrap();
        assert!(result.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
