pub const REASON_INJECTION_BLOCKED: &str = "injection_blocked";
pub const REASON_LOOP_GUARD_TRIGGERED: &str = "loop_guard_triggered";
pub const REASON_CI_GATE_BLOCKED: &str = "ci_gate_blocked";
pub const REASON_ROLLBACK_TRIGGERED: &str = "rollback_triggered";
pub const REASON_SELF_DEV_MODE_RESTRICTION: &str = "self_dev_mode_restriction";
pub const REASON_GHOST_TOOL_UNAVAILABLE: &str = "ghost_tool_unavailable";
pub const REASON_GHOST_RUST_TEMP_UNWRITABLE: &str = "ghost_rust_temp_unwritable";
pub const REASON_GHOST_RUNTIME_CAPABILITY_MISMATCH: &str = "ghost_runtime_capability_mismatch";

pub fn is_known_reason(reason: &str) -> bool {
    matches!(
        reason,
        REASON_INJECTION_BLOCKED
            | REASON_LOOP_GUARD_TRIGGERED
            | REASON_CI_GATE_BLOCKED
            | REASON_ROLLBACK_TRIGGERED
            | REASON_SELF_DEV_MODE_RESTRICTION
            | REASON_GHOST_TOOL_UNAVAILABLE
            | REASON_GHOST_RUST_TEMP_UNWRITABLE
            | REASON_GHOST_RUNTIME_CAPABILITY_MISMATCH
    )
}

pub fn reason_tag(reason: &str) -> String {
    let _known_reason = is_known_reason(reason);
    format!("[reason:{}]", reason)
}

pub fn with_reason(reason: &str, message: impl AsRef<str>) -> String {
    format!("{} {}", reason_tag(reason), message.as_ref())
}

pub fn message_has_reason(message: &str, reason: &str) -> bool {
    message.contains(&reason_tag(reason))
}
