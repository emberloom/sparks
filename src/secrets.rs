use keyring::{Entry, Error as KeyringError};
use rpassword::prompt_password;

use crate::error::{SparksError, Result};

const KEYRING_SERVICE: &str = "sparks";

#[derive(Debug, Clone, Copy)]
pub struct SecretSpec {
    pub key: &'static str,
    pub env: &'static str,
}

#[derive(Debug, Clone)]
pub struct SecretStatus {
    pub key: &'static str,
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
        env: "SPARKS_TELEGRAM_TOKEN",
    },
    SecretSpec {
        key: "telegram.stt_api_key",
        env: "SPARKS_STT_API_KEY",
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
            in_env,
            in_keyring,
        });
    }

    KeyringReport { statuses, error }
}

pub fn set_secret(key: &str) -> Result<()> {
    let spec = find_spec(key)?;
    let prompt = format!("Enter value for {}: ", spec.key);
    let value = prompt_password(prompt)
        .map_err(|e| SparksError::Tool(format!("Failed to read secret from stdin: {}", e)))?;
    if value.trim().is_empty() {
        return Err(SparksError::Tool("Secret cannot be empty".to_string()));
    }
    let entry = Entry::new(KEYRING_SERVICE, spec.key)
        .map_err(|e| SparksError::Tool(format!("Keyring init failed: {}", e)))?;
    entry
        .set_password(&value)
        .map_err(|e| SparksError::Tool(format!("Keyring write failed: {}", e)))?;
    Ok(())
}

pub fn delete_secret(key: &str) -> Result<()> {
    let spec = find_spec(key)?;
    let entry = Entry::new(KEYRING_SERVICE, spec.key)
        .map_err(|e| SparksError::Tool(format!("Keyring init failed: {}", e)))?;
    match entry.delete_credential() {
        Ok(_) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(SparksError::Tool(format!("Keyring delete failed: {}", e))),
    }
}

pub fn find_spec(key: &str) -> Result<SecretSpec> {
    KNOWN_SECRETS
        .iter()
        .copied()
        .find(|spec| spec.key == key)
        .ok_or_else(|| SparksError::Tool(format!("Unknown secret key: {}", key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- KNOWN_SECRETS integrity ---

    #[test]
    fn known_secrets_has_no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for spec in KNOWN_SECRETS {
            assert!(
                seen.insert(spec.key),
                "Duplicate key in KNOWN_SECRETS: {}",
                spec.key
            );
        }
    }

    #[test]
    fn known_secrets_has_no_duplicate_env_vars() {
        let mut seen = std::collections::HashSet::new();
        for spec in KNOWN_SECRETS {
            assert!(
                seen.insert(spec.env),
                "Duplicate env var in KNOWN_SECRETS: {}",
                spec.env
            );
        }
    }

    #[test]
    fn known_secrets_keys_are_non_empty() {
        for spec in KNOWN_SECRETS {
            assert!(!spec.key.is_empty(), "Empty key in KNOWN_SECRETS");
            assert!(
                !spec.env.is_empty(),
                "Empty env var in KNOWN_SECRETS for key {}",
                spec.key
            );
        }
    }

    // --- find_spec ---

    #[test]
    fn find_spec_returns_matching_spec() {
        let spec = find_spec("github.token").expect("github.token should be known");
        assert_eq!(spec.key, "github.token");
        assert_eq!(spec.env, "GH_TOKEN");
    }

    #[test]
    fn find_spec_returns_error_for_unknown_key() {
        let result = find_spec("unknown.key");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown secret key"));
    }

    #[test]
    fn find_spec_covers_all_known_keys() {
        for spec in KNOWN_SECRETS {
            let found = find_spec(spec.key).expect("every KNOWN_SECRETS entry should be findable");
            assert_eq!(found.key, spec.key);
        }
    }
}
