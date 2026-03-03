use async_trait::async_trait;
use std::io::{self, Write};

use crate::error::{AthenaError, Result};

/// Frontend-agnostic confirmation trait.
/// CLI reads from stdin, Telegram sends inline keyboards, etc.
#[async_trait]
pub trait Confirmer: Send + Sync {
    async fn confirm(&self, action: &str) -> Result<bool>;
}

/// CLI confirmer: reads y/N from stdin, supports auto-approve mode.
pub struct CliConfirmer {
    pub auto_approve: bool,
}

#[async_trait]
impl Confirmer for CliConfirmer {
    async fn confirm(&self, action: &str) -> Result<bool> {
        if self.auto_approve {
            eprintln!("⚡ Auto-approved: {}", action);
            return Ok(true);
        }

        let action = action.to_string();
        tokio::task::spawn_blocking(move || {
            eprint!("\n⚠  Action: {}\n   Approve? [y/N] ", action);
            io::stderr()
                .flush()
                .map_err(|e| AthenaError::Tool(format!("Failed to flush prompt: {}", e)))?;

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| AthenaError::Tool(format!("Failed to read input: {}", e)))?;

            let answer = input.trim().to_lowercase();
            if answer == "y" || answer == "yes" {
                Ok(true)
            } else {
                Err(AthenaError::Cancelled)
            }
        })
        .await
        .map_err(|e| AthenaError::Tool(format!("Confirmation task failed: {}", e)))?
    }
}

/// Auto-approve confirmer for autonomous tasks (safety via Docker sandbox).
#[derive(Clone, Copy)]
pub struct AutoConfirmer;

#[async_trait]
impl Confirmer for AutoConfirmer {
    async fn confirm(&self, action: &str) -> Result<bool> {
        tracing::info!(action = %action, "Auto-approved (autonomous task)");
        Ok(true)
    }
}

/// Precompiled sensitive patterns for O(1) matching via RegexSet
pub struct SensitivePatterns {
    regex_set: regex::RegexSet,
}

impl SensitivePatterns {
    /// Compile patterns into a RegexSet. Invalid patterns are logged and skipped.
    pub fn new(patterns: &[String]) -> Self {
        let valid: Vec<&str> = patterns
            .iter()
            .filter(|p| {
                if regex::Regex::new(p).is_err() {
                    tracing::warn!(pattern = %p, "Skipping invalid sensitive pattern");
                    false
                } else {
                    true
                }
            })
            .map(|p| p.as_str())
            .collect();
        let regex_set = match regex::RegexSet::new(&valid) {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!("Failed to compile sensitive pattern set: {}", e);
                regex::RegexSet::empty()
            }
        };
        Self { regex_set }
    }

    /// Check if a command matches any sensitive pattern
    pub fn is_match(&self, cmd: &str) -> bool {
        self.regex_set.is_match(cmd)
    }
}
