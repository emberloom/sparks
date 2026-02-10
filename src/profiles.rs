use std::path::PathBuf;

use crate::config::{AgentConfig, Config, MountConfig};
use crate::error::{AthenaError, Result};

/// A profile file loaded from ~/.athena/agents/*.toml
#[derive(Debug, serde::Deserialize)]
struct AgentProfile {
    name: String,
    description: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default = "default_strategy")]
    strategy: String,
    #[serde(default)]
    mounts: Vec<MountConfig>,
}

fn default_strategy() -> String {
    "react".into()
}

/// Load agents from config + ~/.athena/agents/*.toml profile directory.
/// Profile agents override config agents with the same name.
pub fn load_agents(config: &Config) -> Result<Vec<AgentConfig>> {
    let mut agents: Vec<AgentConfig> = config.agents.clone();

    let profile_dir = match dirs::home_dir() {
        Some(h) => h.join(".athena").join("agents"),
        None => return Ok(agents),
    };

    // Create profile dir if missing
    if !profile_dir.exists() {
        let _ = std::fs::create_dir_all(&profile_dir);
        tracing::info!("Created agent profile directory: {}", profile_dir.display());
        return Ok(agents);
    }

    let entries = match std::fs::read_dir(&profile_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Cannot read profile directory {}: {}", profile_dir.display(), e);
            return Ok(agents);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        match load_profile(&path) {
            Ok(profile) => {
                tracing::info!("Loaded agent profile: {} ({})", profile.name, path.display());

                let agent = AgentConfig {
                    name: profile.name.clone(),
                    description: profile.description,
                    tools: profile.tools,
                    strategy: profile.strategy,
                    mounts: profile.mounts,
                };

                // Deduplicate: profile overrides config agent with same name
                agents.retain(|a| a.name != agent.name);
                agents.push(agent);
            }
            Err(e) => {
                tracing::warn!("Failed to load profile {}: {}", path.display(), e);
            }
        }
    }

    Ok(agents)
}

fn load_profile(path: &PathBuf) -> Result<AgentProfile> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| AthenaError::Config(format!("Failed to read {}: {}", path.display(), e)))?;

    let profile: AgentProfile = toml::from_str(&contents)
        .map_err(|e| AthenaError::Config(format!("Failed to parse {}: {}", path.display(), e)))?;

    Ok(profile)
}
