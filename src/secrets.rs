use keyring::{Entry, Error as KeyringError};
use rpassword::prompt_password;

use crate::error::{AthenaError, Result};

const KEYRING_SERVICE: &str = "athena";

#[derive(Debug, Clone, Copy)]
pub struct SecretSpec {
    pub key: &'static str,
    pub env: &'static str,
}

#[derive(Debug, Clone)]
pub struct SecretStatus {
    pub key: &'static str,
    pub env: &'static str,
    pub in_env: bool,
    pub in_keyring: bool,
}

#[derive(Debug, Clone)]
pub struct KeyringReport {
    pub statuses: Vec<SecretStatus>,
    pub error: Option<String>,
}

pub const KNOWN_SECRETS: &[SecretSpec] = &[
    SecretSpec {
        key: "github.token",
        env: "GH_TOKEN",
    },
    SecretSpec {
        key: "gitlab.token",
        env: "GITLAB_TOKEN",
    },
    SecretSpec {
        key: "linear.api_key",
        env: "LINEAR_API_KEY",
    },
    SecretSpec {
        key: "jira.api_token",
        env: "JIRA_API_TOKEN",
    },
    SecretSpec {
        key: "telegram.token",
        env: "ATHENA_TELEGRAM_TOKEN",
    },
    SecretSpec {
        key: "telegram.stt_api_key",
        env: "ATHENA_STT_API_KEY",
    },
    SecretSpec {
        key: "openrouter.api_key",
        env: "OPENROUTER_API_KEY",
    },
    SecretSpec {
        key: "zen.api_key",
        env: "OPENCODE_API_KEY",
    },
    SecretSpec {
        key: "langfuse.public_key",
        env: "LANGFUSE_PUBLIC_KEY",
    },
    SecretSpec {
        key: "langfuse.secret_key",
        env: "LANGFUSE_SECRET_KEY",
    },
];

pub fn load_keyring_into_env() -> Result<()> {
    for spec in KNOWN_SECRETS {
        if std::env::var(spec.env).is_ok() {
            continue;
        }
        let entry = match Entry::new(KEYRING_SERVICE, spec.key) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Keyring unavailable for {}: {}", spec.key, e);
                continue;
            }
        };
        match entry.get_password() {
            Ok(value) => {
                if !value.trim().is_empty() {
                    std::env::set_var(spec.env, value);
                }
            }
            Err(KeyringError::NoEntry) => {}
            Err(e) => {
                tracing::warn!("Keyring read failed for {}: {}", spec.key, e);
            }
        }
    }

    Ok(())
}

pub fn keyring_report() -> KeyringReport {
    let mut statuses = Vec::new();
    let mut error: Option<String> = None;

    for spec in KNOWN_SECRETS {
        let in_env = std::env::var(spec.env).is_ok();
        let mut in_keyring = false;
        match Entry::new(KEYRING_SERVICE, spec.key) {
            Ok(entry) => match entry.get_password() {
                Ok(_) => in_keyring = true,
                Err(KeyringError::NoEntry) => {}
                Err(e) => {
                    if error.is_none() {
                        error = Some(e.to_string());
                    }
                }
            },
            Err(e) => {
                if error.is_none() {
                    error = Some(e.to_string());
                }
            }
        }
        statuses.push(SecretStatus {
            key: spec.key,
            env: spec.env,
            in_env,
            in_keyring,
        });
    }

    KeyringReport { statuses, error }
}

pub fn set_secret(key: &str) -> Result<()> {
    let spec = find_spec(key)?;
    let prompt = format!("Enter value for {}: ", spec.key);
    let value = prompt_password(prompt).map_err(|e| {
        AthenaError::Tool(format!("Failed to read secret from stdin: {}", e))
    })?;
    if value.trim().is_empty() {
        return Err(AthenaError::Tool("Secret cannot be empty".to_string()));
    }
    let entry = Entry::new(KEYRING_SERVICE, spec.key)
        .map_err(|e| AthenaError::Tool(format!("Keyring init failed: {}", e)))?;
    entry
        .set_password(&value)
        .map_err(|e| AthenaError::Tool(format!("Keyring write failed: {}", e)))?;
    Ok(())
}

pub fn delete_secret(key: &str) -> Result<()> {
    let spec = find_spec(key)?;
    let entry = Entry::new(KEYRING_SERVICE, spec.key)
        .map_err(|e| AthenaError::Tool(format!("Keyring init failed: {}", e)))?;
    match entry.delete_credential() {
        Ok(_) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(AthenaError::Tool(format!("Keyring delete failed: {}", e))),
    }
}

pub fn find_spec(key: &str) -> Result<SecretSpec> {
    KNOWN_SECRETS
        .iter()
        .copied()
        .find(|spec| spec.key == key)
        .ok_or_else(|| AthenaError::Tool(format!("Unknown secret key: {}", key)))
}
