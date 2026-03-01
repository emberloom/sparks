use crate::error::AthenaError;
use crate::memory::MemoryStore;
use crate::strategy::TaskContract;
use serde_json::Value;

fn make_contract(
    context: String,
    goal: String,
    constraints: Vec<String>,
    soul: String,
) -> TaskContract {
    TaskContract {
        context,
        goal,
        constraints,
        soul: Some(soul),
        tools_doc: None,
        cli_tool_preference: None,
        test_generation: false,
        memory: None,
    }
}

fn truncate_for_memory(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn has_test_failures(test_output: &str) -> bool {
    test_output.contains("test result: FAILED")
        || test_output.contains("FAILED")
        || test_output.contains("error[E")
}

pub fn classify_test_failure_category(test_output: &str) -> &'static str {
    if test_output.contains("error[E0308]")
        || test_output.contains("error[E0599]")
        || test_output.contains("TypeError")
    {
        "type_error"
    } else if test_output.contains("error[E0432]")
        || test_output.contains("error[E0433]")
        || test_output.contains("ImportError")
    {
        "import_error"
    } else if test_output.contains("panicked at") || test_output.contains("panic!") {
        "panic"
    } else if test_output.contains("assertion `")
        || test_output.contains("assert_eq!")
        || test_output.contains("assertion failed")
    {
        "assertion_failure"
    } else {
        "general_test_failure"
    }
}

pub fn encode_self_heal_outcome(
    error_category: &str,
    fix_attempted: &str,
    success: bool,
) -> String {
    serde_json::json!({
        "error_category": error_category,
        "fix_attempted": truncate_for_memory(fix_attempted, 400),
        "success": success
    })
    .to_string()
}

pub fn decode_self_heal_outcome(content: &str) -> Option<(String, String, bool)> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    let category = json.get("error_category")?.as_str()?.to_string();
    let fix_attempted = json.get("fix_attempted")?.as_str()?.to_string();
    let success = json.get("success")?.as_bool()?;
    Some((category, fix_attempted, success))
}

pub fn store_self_heal_outcome(
    memory: &MemoryStore,
    error_category: &str,
    fix_attempted: &str,
    success: bool,
) {
    let content = encode_self_heal_outcome(error_category, fix_attempted, success);
    if let Err(e) = memory.store("self_heal_outcome", &content, None) {
        tracing::warn!("Failed to store self-heal outcome memory: {}", e);
    }
}

pub fn find_successful_fix_pattern(memory: &MemoryStore, error_category: &str) -> Option<String> {
    let outcomes = memory.search("self_heal_outcome").ok()?;
    outcomes
        .into_iter()
        .filter(|m| m.category == "self_heal_outcome")
        .find_map(|m| {
            let (category, fix_attempted, success) = decode_self_heal_outcome(&m.content)?;
            if category == error_category && success {
                Some(fix_attempted)
            } else {
                None
            }
        })
}

/// Handles web_fetch, file_edit, shell permission, shell command-not-found, and file I/O errors.
fn fix_tool_error_io(
    message: &str,
    message_lower: &str,
    original_tool_name: &str,
    params_pretty: &str,
    original_tool_params: &Value,
) -> Option<TaskContract> {
    if message.contains("web_fetch")
        && (message.contains("timed out") || message.contains("timeout"))
    {
        let context = format!(
            "The tool '{}' failed with a timeout error when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The `web_fetch` tool failed with a timeout. This is likely because the default reqwest client has no timeout configured. Modify `src/tools.rs` to fix this. Find the `WebFetchTool` implementation and its `execute` method. Create a `reqwest::Client` with a reasonable timeout (e.g., 30 seconds) and use that for the request. After editing, run `cargo check` to ensure the code still compiles.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Only modify the `WebFetchTool` in `src/tools.rs`.".to_string(),
                "Ensure the change is minimal and focused on adding a timeout."
                    .to_string(),
                "Verify the code compiles with `cargo check` after the change."
                    .to_string(),
            ],
            "You are a senior Rust developer ghost, specialized in fixing bugs and improving code robustness. Your goal is to apply a targeted fix to resolve a tool timeout issue.".to_string(),
        ))
    } else if original_tool_name == "file_edit"
        && (message.contains("not found") || message.contains("must be unique"))
    {
        let path = original_tool_params["path"].as_str().unwrap_or_default();
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

        Some(make_contract(
            context,
            goal,
            vec![
                "Read the file before attempting to edit.".to_string(),
                "Ensure the new `old_string` is unique within the file.".to_string(),
                "Preserve the original `new_string` — only fix the `old_string`.".to_string(),
            ],
            "You are a senior Rust developer ghost, specialized in precise \
             code edits. Your goal is to retry a failed file_edit with a \
             corrected old_string that exactly matches the file contents."
                .to_string(),
        ))
    } else if original_tool_name == "shell" && message_lower.contains("permission denied") {
        let context = format!(
            "The tool '{}' failed with a permission error when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The shell command failed due to permission denied. Identify the file or command causing the permission issue and fix it by adjusting permissions (e.g., chmod), using the correct user, or selecting an alternative path. Re-run the command afterward to confirm it succeeds.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Prefer the least-privilege change necessary.".to_string(),
                "Avoid broad permission changes unless clearly required."
                    .to_string(),
                "Verify the command succeeds after the fix.".to_string(),
            ],
            "You are a senior engineer ghost focused on safe, minimal fixes. Resolve the permission issue without unnecessary changes.".to_string(),
        ))
    } else if original_tool_name == "shell"
        && (message_lower.contains("command not found")
            || (message_lower.contains("not found") && !message_lower.contains("no such file")))
    {
        let context = format!(
            "The tool '{}' failed because a command was not found when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The shell command was not found. Identify the missing tool or command, then either install it (if appropriate for this environment) or switch to an available alternative. Update the command accordingly and re-run it.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Prefer using tools already available in the environment."
                    .to_string(),
                "If installing is required, use the minimal necessary steps."
                    .to_string(),
                "Re-run the command to confirm it works.".to_string(),
            ],
            "You are a senior engineer ghost specialized in tooling issues. Resolve missing commands pragmatically and verify the fix.".to_string(),
        ))
    } else {
        None
    }
}

/// Handles file I/O, compiler errors, import errors, HTTP status errors, OOM, and search failures.
fn fix_tool_error_build(
    message: &str,
    message_lower: &str,
    original_tool_name: &str,
    params_pretty: &str,
) -> Option<TaskContract> {
    if (original_tool_name == "file_read" || original_tool_name == "file_write")
        && (message_lower.contains("no such file") || message_lower.contains("not found"))
    {
        let context = format!(
            "The tool '{}' failed because a file was not found when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The file path was not found. Locate the correct file path using `rg`, `glob`, or directory listings. If needed, create parent directories before writing. Update the path and retry the operation.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Do not create new files unless required.".to_string(),
                "Prefer locating the correct existing path first.".to_string(),
                "If writing, ensure parent directories exist.".to_string(),
            ],
            "You are a senior engineer ghost focused on precise file operations. Identify the correct path and retry safely.".to_string(),
        ))
    } else if (original_tool_name == "shell" || original_tool_name == "lint")
        && message.contains("error[E")
    {
        let context = format!(
            "The tool '{}' failed with a Rust compiler error when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "A Rust compiler error occurred (error[E...]). Parse the error code and message, open the referenced file/line, and fix the specific compilation issue. Re-run `cargo check` (or the original command) to confirm it compiles.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Fix the specific error code indicated by rustc.".to_string(),
                "Keep the change minimal and localized.".to_string(),
                "Re-run `cargo check` after the fix.".to_string(),
            ],
            "You are a senior Rust developer ghost focused on compiler errors. Apply a precise fix based on the error code.".to_string(),
        ))
    } else if (original_tool_name == "shell" || original_tool_name == "lint")
        && (message.contains("ModuleNotFoundError")
            || message.contains("Cannot find module")
            || message.contains("unresolved import"))
    {
        let context = format!(
            "The tool '{}' failed due to an import or module resolution error when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "An import or module resolution error occurred. Verify the module or package exists, check the import path, and fix the reference. If a dependency is missing, add it in the appropriate manifest. Re-run the original command to confirm it works.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Prefer fixing the import path before adding new dependencies."
                    .to_string(),
                "Keep dependency changes minimal if required.".to_string(),
                "Re-run the command after the fix.".to_string(),
            ],
            "You are a senior engineer ghost focused on dependency issues. Resolve the import failure with minimal, correct changes.".to_string(),
        ))
    } else {
        fix_tool_error_env(message, message_lower, original_tool_name, params_pretty)
    }
}

/// Handles HTTP status errors, resource exhaustion, and search failures.
fn fix_tool_error_env(
    message: &str,
    message_lower: &str,
    original_tool_name: &str,
    params_pretty: &str,
) -> Option<TaskContract> {
    if message.contains("web_fetch")
        && (message.contains("403")
            || message.contains("404")
            || message.contains("401")
            || message_lower.contains("status:"))
    {
        let context = format!(
            "A web_fetch call returned an HTTP error status when called with parameters: {}.\nError message: {}",
            params_pretty, message
        );

        let goal = "The `web_fetch` request returned an HTTP error status. Check the URL for correctness, verify authentication or headers if needed, and consider alternative endpoints. Ensure the calling code handles the HTTP error gracefully.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Do not hardcode secrets or tokens.".to_string(),
                "Verify the URL and endpoint correctness.".to_string(),
                "Handle the HTTP error explicitly.".to_string(),
            ],
            "You are a senior engineer ghost specializing in HTTP troubleshooting. Diagnose the status error and fix the request or handling.".to_string(),
        ))
    } else if original_tool_name == "shell"
        && (message_lower.contains("out of memory")
            || message_lower.contains("oom")
            || message_lower.contains("cannot allocate")
            || message_lower.contains("killed"))
    {
        let context = format!(
            "The tool '{}' failed due to resource limits when called with parameters: {}.\nError message: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The shell command appears to have exceeded memory or resource limits. Reduce the scope (e.g., fewer files, smaller batch size), split the work into smaller steps, or adjust command flags to use less memory. Re-run the command after reducing resource usage.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Prefer reducing scope over increasing resource limits."
                    .to_string(),
                "Split large operations into smaller chunks.".to_string(),
                "Re-run the command to verify success.".to_string(),
            ],
            "You are a senior engineer ghost focused on reliable execution. Reduce resource usage and retry safely.".to_string(),
        ))
    } else if (original_tool_name == "grep" || original_tool_name == "glob")
        && (message_lower.contains("no matches") || message.trim().is_empty())
    {
        let context = format!(
            "The tool '{}' returned no matches when called with parameters: {}.\nOutput: {}",
            original_tool_name, params_pretty, message
        );

        let goal = "The search returned no matches. Broaden the search pattern, verify the search path, or try alternative patterns. Then re-run the search to locate the intended targets.".to_string();

        Some(make_contract(
            context,
            goal,
            vec![
                "Check the search path for correctness.".to_string(),
                "Try a broader or different pattern.".to_string(),
                "Re-run the search after adjustments.".to_string(),
            ],
            "You are a senior engineer ghost focused on fast discovery. Adjust the search to find the intended matches.".to_string(),
        ))
    } else {
        None
    }
}

/// Analyzes a tool execution error and attempts to generate a self-healing task.
pub fn attempt_fix(
    error: &AthenaError,
    original_tool_name: &str,
    original_tool_params: &Value,
) -> Option<TaskContract> {
    match error {
        AthenaError::Tool(message) => {
            let message_lower = message.to_lowercase();
            let params_pretty =
                serde_json::to_string_pretty(original_tool_params).unwrap_or_default();
            fix_tool_error_io(
                message,
                &message_lower,
                original_tool_name,
                &params_pretty,
                original_tool_params,
            )
            .or_else(|| {
                fix_tool_error_build(message, &message_lower, original_tool_name, &params_pretty)
            })
        }
        AthenaError::Timeout(seconds) => {
            let context = format!(
                "The operation timed out after {}s while calling tool '{}' with parameters: {}.",
                seconds,
                original_tool_name,
                serde_json::to_string_pretty(original_tool_params).unwrap_or_default()
            );

            let goal = "The operation timed out. Reduce the scope of the task, break it into smaller steps, or adjust timeouts if available. Re-run the operation to confirm it completes within limits.".to_string();

            Some(make_contract(
                context,
                goal,
                vec![
                    "Prefer reducing scope over increasing timeouts."
                        .to_string(),
                    "Split large tasks into smaller steps.".to_string(),
                    "Re-run the operation to verify completion.".to_string(),
                ],
                "You are a senior engineer ghost focused on reliability. Adjust the workflow to avoid timeouts.".to_string(),
            ))
        }
        AthenaError::Docker(message) => {
            let context = format!(
                "A Docker-related error occurred while calling tool '{}' with parameters: {}.\nError message: {}",
                original_tool_name,
                serde_json::to_string_pretty(original_tool_params).unwrap_or_default(),
                message
            );

            let goal = "A Docker error occurred. Check container state, image availability, and Docker daemon health. If needed, restart or recreate the container and re-run the operation.".to_string();

            Some(make_contract(
                context,
                goal,
                vec![
                    "Verify the container is running and healthy.".to_string(),
                    "Avoid broad system changes unless required.".to_string(),
                    "Re-run the original operation after fixing Docker."
                        .to_string(),
                ],
                "You are a senior engineer ghost specialized in container issues. Restore Docker functionality and retry.".to_string(),
            ))
        }
        _ => None,
    }
}

/// Analyzes test output and attempts to generate a corrective task.
/// Called when the VERIFY phase detects test failures.
pub fn attempt_test_fix(test_output: &str, original_goal: &str) -> Option<TaskContract> {
    if !has_test_failures(test_output) {
        return None;
    }

    let context = format!(
        "Tests failed after implementing: {}\n\nTest output:\n{}",
        original_goal,
        if test_output.len() > 4000 {
            &test_output[..4000]
        } else {
            test_output
        }
    );

    if test_output.contains("error[E0308]")
        || test_output.contains("error[E0599]")
        || test_output.contains("TypeError")
    {
        let goal = "Tests failed with a type error. Analyze the type mismatch or missing method indicated in the output, review the relevant function signatures and return types, and update the implementation to satisfy the expected types. Re-run the tests to confirm they pass.".to_string();

        return Some(make_contract(
            context.clone(),
            goal,
            vec![
                "Focus on type signatures and return types.".to_string(),
                "Fix the implementation, not the tests.".to_string(),
                "Re-run tests after the fix.".to_string(),
            ],
            "You are a senior developer ghost specialized in type-system debugging. Resolve the type error with a minimal, correct change.".to_string(),
        ));
    }

    if test_output.contains("error[E0432]")
        || test_output.contains("error[E0433]")
        || test_output.contains("ImportError")
    {
        let goal = "Tests failed with an import or module resolution error. Inspect module structure, verify the import paths, and fix the `use` statements or dependency declarations. Re-run tests after correcting the imports.".to_string();

        return Some(make_contract(
            context.clone(),
            goal,
            vec![
                "Verify module paths before adding new dependencies."
                    .to_string(),
                "Fix the implementation, not the tests.".to_string(),
                "Re-run tests after the fix.".to_string(),
            ],
            "You are a senior developer ghost focused on module resolution issues. Correct the import paths precisely.".to_string(),
        ));
    }

    let goal = "Tests are failing after a code change. Analyze the test output, \
        identify the root cause, and fix the IMPLEMENTATION (not the tests). \
        The tests define the expected behavior — the code must conform to them. \
        After fixing, run the tests again to confirm they pass."
        .to_string();

    Some(make_contract(
        context,
        goal,
        vec![
            "Fix the implementation, NOT the tests.".to_string(),
            "Run tests after fixing to verify.".to_string(),
            "Keep changes minimal and focused on the failing tests.".to_string(),
        ],
        "You are a senior developer ghost specialized in debugging test failures. \
         Analyze test output carefully, identify root causes in the implementation, \
         and apply targeted fixes."
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_test_failure_category, decode_self_heal_outcome, encode_self_heal_outcome,
        has_test_failures,
    };

    #[test]
    fn has_test_failures_detects_key_patterns() {
        assert!(has_test_failures("test result: FAILED. 1 passed; 1 failed"));
        assert!(has_test_failures("error[E0308]: mismatched types"));
        assert!(!has_test_failures("test result: ok. 2 passed; 0 failed"));
    }

    #[test]
    fn classify_test_failure_category_detects_type_and_import() {
        assert_eq!(
            classify_test_failure_category("error[E0308]: mismatched types"),
            "type_error"
        );
        assert_eq!(
            classify_test_failure_category("error[E0432]: unresolved import"),
            "import_error"
        );
    }

    #[test]
    fn self_heal_outcome_round_trip() {
        let encoded = encode_self_heal_outcome("type_error", "fix signature", true);
        let decoded = decode_self_heal_outcome(&encoded).expect("decode should succeed");
        assert_eq!(decoded.0, "type_error");
        assert_eq!(decoded.1, "fix signature");
        assert!(decoded.2);
    }
}
