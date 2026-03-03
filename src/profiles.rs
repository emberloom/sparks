use std::path::PathBuf;

use crate::config::{Config, GhostConfig, MountConfig};
use crate::error::{AthenaError, Result};

/// A profile file loaded from ~/.athena/ghosts/*.toml
#[derive(Debug, serde::Deserialize)]
struct GhostProfile {
    name: String,
    description: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default = "default_strategy")]
    strategy: String,
    #[serde(default)]
    mounts: Vec<MountConfig>,
    /// Path to a soul file (markdown identity document)
    soul_file: Option<String>,
    /// Docker image override
    image: Option<String>,
}

fn default_strategy() -> String {
    "react".into()
}

fn is_legacy_rust_image(image: &str) -> bool {
    image.starts_with("rust:1.84")
        || image.starts_with("rust:1.85")
        || image.starts_with("rust:1.86")
        || image.starts_with("rust:1.87")
}

fn normalize_profile_image(image: Option<String>, fallback_image: &str) -> Option<String> {
    match image {
        Some(img) if is_legacy_rust_image(&img) && fallback_image != img => {
            tracing::warn!(
                profile_image = %img,
                fallback_image = fallback_image,
                "Normalizing legacy ghost image to configured docker.image (Rust >=1.88 required)"
            );
            Some(fallback_image.to_string())
        }
        other => other,
    }
}

fn home_profile_loading_disabled() -> bool {
    std::env::var("ATHENA_DISABLE_HOME_PROFILES")
        .ok()
        .map(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Load ghosts from config + ~/.athena/ghosts/*.toml profile directory.
/// Profile ghosts override config ghosts with the same name.
pub fn load_ghosts(config: &Config) -> Result<Vec<GhostConfig>> {
    let mut ghosts: Vec<GhostConfig> = config.ghosts.clone();

    if home_profile_loading_disabled() {
        tracing::info!("Home ghost profiles disabled via ATHENA_DISABLE_HOME_PROFILES");
        return Ok(ghosts);
    }

    let profile_dir = match dirs::home_dir() {
        Some(h) => h.join(".athena").join("ghosts"),
        None => return Ok(ghosts),
    };

    // Create profile dir if missing
    if !profile_dir.exists() {
        let _ = std::fs::create_dir_all(&profile_dir);
        tracing::info!("Created ghost profile directory: {}", profile_dir.display());
        return Ok(ghosts);
    }

    let entries = match std::fs::read_dir(&profile_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "Cannot read profile directory {}: {}",
                profile_dir.display(),
                e
            );
            return Ok(ghosts);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        match load_profile(&path) {
            Ok(profile) => {
                tracing::info!(
                    "Loaded ghost profile: {} ({})",
                    profile.name,
                    path.display()
                );

                let soul =
                    profile.soul_file.as_ref().and_then(
                        |path| match crate::config::load_soul_file(path) {
                            Ok(content) => {
                                tracing::info!(
                                    "Loaded soul for profile ghost '{}' from {}",
                                    profile.name,
                                    path
                                );
                                Some(content)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to load soul for profile ghost '{}': {}",
                                    profile.name,
                                    e
                                );
                                None
                            }
                        },
                    );

                let ghost = GhostConfig {
                    name: profile.name.clone(),
                    description: profile.description,
                    tools: profile.tools,
                    strategy: profile.strategy,
                    mounts: profile.mounts,
                    soul_file: profile.soul_file,
                    soul,
                    image: normalize_profile_image(profile.image, &config.docker.image),
                };

                // Deduplicate: profile overrides config ghost with same name
                ghosts.retain(|g| g.name != ghost.name);
                ghosts.push(ghost);
            }
            Err(e) => {
                tracing::warn!("Failed to load profile {}: {}", path.display(), e);
            }
        }
    }

    Ok(ghosts)
}

fn load_profile(path: &PathBuf) -> Result<GhostProfile> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| AthenaError::Config(format!("Failed to read {}: {}", path.display(), e)))?;

    let profile: GhostProfile = toml::from_str(&contents)
        .map_err(|e| AthenaError::Config(format!("Failed to parse {}: {}", path.display(), e)))?;

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::normalize_profile_image;

    #[test]
    fn normalizes_legacy_rust_184_profile_image() {
        let normalized = normalize_profile_image(Some("rust:1.84-slim".to_string()), "rust:1.93");
        assert_eq!(normalized.as_deref(), Some("rust:1.93"));
    }

    #[test]
    fn normalizes_legacy_rust_187_profile_image() {
        let normalized = normalize_profile_image(Some("rust:1.87-slim".to_string()), "rust:1.93");
        assert_eq!(normalized.as_deref(), Some("rust:1.93"));
    }

    #[test]
    fn preserves_non_legacy_profile_image() {
        let normalized = normalize_profile_image(Some("rust:1.93".to_string()), "rust:1.93");
        assert_eq!(normalized.as_deref(), Some("rust:1.93"));
    }

    #[test]
    fn preserves_missing_profile_image() {
        let normalized = normalize_profile_image(None, "rust:1.93");
        assert_eq!(normalized, None);
    }
}
