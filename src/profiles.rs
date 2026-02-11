use std::path::PathBuf;

use crate::config::{GhostConfig, Config, MountConfig};
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
}

fn default_strategy() -> String {
    "react".into()
}

/// Load ghosts from config + ~/.athena/ghosts/*.toml profile directory.
/// Profile ghosts override config ghosts with the same name.
pub fn load_ghosts(config: &Config) -> Result<Vec<GhostConfig>> {
    let mut ghosts: Vec<GhostConfig> = config.ghosts.clone();

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
            tracing::warn!("Cannot read profile directory {}: {}", profile_dir.display(), e);
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
                tracing::info!("Loaded ghost profile: {} ({})", profile.name, path.display());

                let ghost = GhostConfig {
                    name: profile.name.clone(),
                    description: profile.description,
                    tools: profile.tools,
                    strategy: profile.strategy,
                    mounts: profile.mounts,
                    soul_file: profile.soul_file,
                    soul: None,
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
