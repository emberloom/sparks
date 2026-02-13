use crate::error::AthenaError;
use crate::strategy::TaskContract;
use serde_json::Value;

/// Analyzes a tool execution error and attempts to generate a self-healing task.
pub fn attempt_fix(
    error: &AthenaError,
    original_tool_name: &str,
    original_tool_params: &Value,
) -> Option<TaskContract> {
    match error {
        AthenaError::Tool(message) => {
            if message.contains("web_fetch") && (message.contains("timed out") || message.contains("timeout")) {
                let context = format!(
                    "The tool '{}' failed with a timeout error when called with parameters: {}.\nError message: {}",
                    original_tool_name,
                    serde_json::to_string_pretty(original_tool_params).unwrap_or_default(),
                    message
                );

                let goal = "The `web_fetch` tool failed with a timeout. This is likely because the default reqwest client has no timeout configured. Modify `src/tools.rs` to fix this. Find the `WebFetchTool` implementation and its `execute` method. Create a `reqwest::Client` with a reasonable timeout (e.g., 30 seconds) and use that for the request. After editing, run `cargo check` to ensure the code still compiles.".to_string();

                Some(TaskContract {
                    context,
                    goal,
                    constraints: vec![
                        "Only modify the `WebFetchTool` in `src/tools.rs`.".to_string(),
                        "Ensure the change is minimal and focused on adding a timeout.".to_string(),
                        "Verify the code compiles with `cargo check` after the change.".to_string(),
                    ],
                    soul: Some("You are a senior Rust developer ghost, specialized in fixing bugs and improving code robustness. Your goal is to apply a targeted fix to resolve a tool timeout issue.".to_string()),
                    tools_doc: None, // Let the default be used
                    cli_tool_preference: None,
                })
            } else if original_tool_name == "file_edit"
                && (message.contains("not found") || message.contains("must be unique"))
            {
                let path = original_tool_params["path"]
                    .as_str()
                    .unwrap_or_default();
                let old_string = original_tool_params["old_string"]
                    .as_str()
                    .unwrap_or_default();
                let new_string = original_tool_params["new_string"]
                    .as_str()
                    .unwrap_or_default();

                let context = format!(
                    "A `file_edit` tool call failed.\n\
                     Path: {}\n\
                     old_string: {:?}\n\
                     new_string: {:?}\n\
                     Error: {}",
                    path, old_string, new_string, message
                );

                let goal = "First, read the file at the given path. Then, call `file_edit` \
                    again with a corrected `old_string` that is guaranteed to be unique, \
                    using the original `new_string`. The original `old_string` was likely \
                    incorrect or not specific enough."
                    .to_string();

                Some(TaskContract {
                    context,
                    goal,
                    constraints: vec![
                        "Read the file before attempting to edit.".to_string(),
                        "Ensure the new `old_string` is unique within the file.".to_string(),
                        "Preserve the original `new_string` — only fix the `old_string`.".to_string(),
                    ],
                    soul: Some(
                        "You are a senior Rust developer ghost, specialized in precise \
                         code edits. Your goal is to retry a failed file_edit with a \
                         corrected old_string that exactly matches the file contents."
                            .to_string(),
                    ),
                    tools_doc: None,
                    cli_tool_preference: None,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}
