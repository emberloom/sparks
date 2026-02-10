use std::io::{self, Write};

use crate::error::{AthenaError, Result};

/// Prompt user for confirmation of a sensitive action.
/// Returns Ok(true) if approved, Err(Cancelled) if denied.
pub fn confirm(action: &str) -> Result<bool> {
    print!("\n⚠  Action: {}\n   Approve? [y/N] ", action);
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input)
        .map_err(|e| AthenaError::Tool(format!("Failed to read input: {}", e)))?;

    let answer = input.trim().to_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(true)
    } else {
        Err(AthenaError::Cancelled)
    }
}

/// Check if a command matches any sensitive patterns
pub fn is_sensitive(cmd: &str, patterns: &[String]) -> bool {
    for pat in patterns {
        if let Ok(re) = regex::Regex::new(pat) {
            if re.is_match(cmd) {
                return true;
            }
        }
    }
    false
}
