use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::error::{AthenaError, Result};
use crate::llm::{LlmProvider, OllamaClient, OpenAiCompatibleClient, OpenAiCompatibleConfig};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub ollama: OllamaConfig,
    #[serde(default)]
    pub openrouter: Option<OpenRouterConfig>,
    #[serde(default)]
    pub zen: Option<ZenConfig>,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub manager: ManagerConfig,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub telegram: TelegramConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self { provider: default_provider() }
    }
}

fn default_provider() -> String { "ollama".into() }

#[derive(Debug, Deserialize, Clone)]
pub struct OpenRouterConfig {
    #[serde(default = "default_openrouter_url")]
    pub url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub classifier_model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_openrouter_url() -> String { "https://openrouter.ai/api/v1".into() }

#[derive(Debug, Deserialize, Clone)]
pub struct ZenConfig {
    #[serde(default = "default_zen_url")]
    pub url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub classifier_model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_zen_url() -> String { "https://opencode.ai/zen/v1".into() }

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    /// Bot token (or set ATHENA_TELEGRAM_TOKEN env var)
    pub token: Option<String>,
    /// Allowed chat IDs (empty = allow all, logs warning)
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            token: None,
            allowed_chats: vec![],
            confirm_timeout_secs: default_confirm_timeout(),
        }
    }
}

fn default_confirm_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone)]
pub struct OllamaConfig {
    #[serde(default = "default_ollama_url")]
    pub url: String,
    #[serde(default = "default_model")]
    pub model: String,
    pub classifier_model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DockerConfig {
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_runtime")]
    pub runtime: String,
    #[serde(default = "default_memory_limit")]
    pub memory_limit: i64,
    #[serde(default = "default_cpu_quota")]
    pub cpu_quota: i64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DbConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManagerConfig {
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_sensitive_patterns")]
    pub sensitive_patterns: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MountConfig {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

// Defaults
fn default_ollama_url() -> String { "http://localhost:11434".into() }
fn default_model() -> String { "llama3.2".into() }
fn default_temperature() -> f32 { 0.3 }
fn default_max_tokens() -> u32 { 4096 }
fn default_image() -> String { "ubuntu:24.04".into() }
fn default_socket_path() -> String { "/var/run/docker.sock".into() }
fn default_runtime() -> String { "runc".into() }
fn default_memory_limit() -> i64 { 268_435_456 } // 256MB
fn default_cpu_quota() -> i64 { 50_000 }
fn default_timeout_secs() -> u64 { 120 }
fn default_db_path() -> String { "~/.athena/athena.db".into() }
fn default_max_steps() -> usize { 15 }
fn default_strategy() -> String { "react".into() }
fn default_sensitive_patterns() -> Vec<String> {
    vec![
        r"rm\s".into(),
        r"sudo".into(),
        r"curl.*\|.*sh".into(),
        r"chmod.*777".into(),
    ]
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            url: default_ollama_url(),
            model: default_model(),
            classifier_model: None,
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: default_image(),
            socket_path: default_socket_path(),
            runtime: default_runtime(),
            memory_limit: default_memory_limit(),
            cpu_quota: default_cpu_quota(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self { path: default_db_path() }
    }
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            sensitive_patterns: default_sensitive_patterns(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            ollama: OllamaConfig::default(),
            openrouter: None,
            zen: None,
            docker: DockerConfig::default(),
            db: DbConfig::default(),
            manager: ManagerConfig::default(),
            agents: default_agents(),
            telegram: TelegramConfig::default(),
        }
    }
}

fn default_agents() -> Vec<AgentConfig> {
    vec![
        AgentConfig {
            name: "coder".into(),
            description: "Reads, writes, and modifies code files. Can run build/test commands.".into(),
            tools: vec!["file_read".into(), "file_write".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: false,
            }],
            strategy: "react".into(),
        },
        AgentConfig {
            name: "scout".into(),
            description: "Explores files and runs read-only commands. Cannot modify anything.".into(),
            tools: vec!["file_read".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: true,
            }],
            strategy: "react".into(),
        },
    ]
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = if let Some(p) = path {
            p.to_path_buf()
        } else {
            // Look in current directory, then ~/.athena/
            let local = PathBuf::from("config.toml");
            if local.exists() {
                local
            } else {
                let home = dirs::home_dir()
                    .ok_or_else(|| AthenaError::Config("Cannot find home directory".into()))?;
                let global = home.join(".athena").join("config.toml");
                if global.exists() {
                    global
                } else {
                    tracing::info!("No config file found, using defaults");
                    return Ok(Config::default());
                }
            }
        };

        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| AthenaError::Config(format!("Failed to read {}: {}", config_path.display(), e)))?;

        let config: Config = toml::from_str(&contents)
            .map_err(|e| AthenaError::Config(format!("Failed to parse config: {}", e)))?;

        Ok(config)
    }

    /// Build the configured LLM provider
    pub fn build_llm_provider(&self) -> Result<Arc<dyn LlmProvider>> {
        match self.llm.provider.as_str() {
            "ollama" => Ok(Arc::new(OllamaClient::new(self.ollama.clone()))),
            "openrouter" => {
                let cfg = self.openrouter.as_ref()
                    .ok_or_else(|| AthenaError::Config(
                        "provider = \"openrouter\" but [openrouter] section is missing".into()
                    ))?;
                let api_key = cfg.api_key.clone()
                    .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                    .ok_or_else(|| AthenaError::Config(
                        "OpenRouter API key not set (config api_key or OPENROUTER_API_KEY env)".into()
                    ))?;
                Ok(Arc::new(OpenAiCompatibleClient::new(
                    OpenAiCompatibleConfig {
                        url: cfg.url.clone(),
                        api_key,
                        model: cfg.model.clone(),
                        temperature: cfg.temperature,
                        max_tokens: cfg.max_tokens,
                    },
                    "OpenRouter",
                )))
            }
            "zen" => {
                let cfg = self.zen.as_ref()
                    .ok_or_else(|| AthenaError::Config(
                        "provider = \"zen\" but [zen] section is missing".into()
                    ))?;
                let api_key = cfg.api_key.clone()
                    .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                    .ok_or_else(|| AthenaError::Config(
                        "Opencode Zen API key not set (config api_key or OPENCODE_API_KEY env)".into()
                    ))?;
                Ok(Arc::new(OpenAiCompatibleClient::new(
                    OpenAiCompatibleConfig {
                        url: cfg.url.clone(),
                        api_key,
                        model: cfg.model.clone(),
                        temperature: cfg.temperature,
                        max_tokens: cfg.max_tokens,
                    },
                    "Opencode Zen",
                )))
            }
            other => Err(AthenaError::Config(format!("Unknown LLM provider: {}", other))),
        }
    }

    /// Build the classifier LLM provider (falls back to main provider if no classifier_model set)
    pub fn build_classifier_provider(&self, fallback: &Arc<dyn LlmProvider>) -> Result<Arc<dyn LlmProvider>> {
        match self.llm.provider.as_str() {
            "ollama" => {
                match &self.ollama.classifier_model {
                    Some(model) => {
                        let mut cfg = self.ollama.clone();
                        cfg.model = model.clone();
                        Ok(Arc::new(OllamaClient::new(cfg)))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            "openrouter" => {
                let cfg = self.openrouter.as_ref().unwrap(); // already validated in build_llm_provider
                match &cfg.classifier_model {
                    Some(model) => {
                        let api_key = cfg.api_key.clone()
                            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                            .unwrap(); // already validated
                        Ok(Arc::new(OpenAiCompatibleClient::new(
                            OpenAiCompatibleConfig {
                                url: cfg.url.clone(),
                                api_key,
                                model: model.clone(),
                                temperature: cfg.temperature,
                                max_tokens: cfg.max_tokens,
                            },
                            "OpenRouter/classifier",
                        )))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            "zen" => {
                let cfg = self.zen.as_ref().unwrap(); // already validated
                match &cfg.classifier_model {
                    Some(model) => {
                        let api_key = cfg.api_key.clone()
                            .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                            .unwrap(); // already validated
                        Ok(Arc::new(OpenAiCompatibleClient::new(
                            OpenAiCompatibleConfig {
                                url: cfg.url.clone(),
                                api_key,
                                model: model.clone(),
                                temperature: cfg.temperature,
                                max_tokens: cfg.max_tokens,
                            },
                            "Zen/classifier",
                        )))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            _ => Ok(fallback.clone()),
        }
    }

    /// Resolve the db path, expanding ~ to home dir
    pub fn db_path(&self) -> Result<PathBuf> {
        let path = if self.db.path.starts_with("~/") {
            let home = dirs::home_dir()
                .ok_or_else(|| AthenaError::Config("Cannot find home directory".into()))?;
            home.join(&self.db.path[2..])
        } else {
            PathBuf::from(&self.db.path)
        };
        Ok(path)
    }

    /// Resolve a mount host_path relative to cwd
    pub fn resolve_mount_path(host_path: &str) -> String {
        if host_path == "." {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".into())
        } else if host_path.starts_with("~/") {
            dirs::home_dir()
                .map(|h| h.join(&host_path[2..]).to_string_lossy().to_string())
                .unwrap_or_else(|| host_path.into())
        } else {
            host_path.into()
        }
    }
}
