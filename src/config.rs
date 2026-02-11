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
    pub ghosts: Vec<GhostConfig>,
    #[serde(default)]
    pub persona: PersonaConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub mood: MoodConfig,
    #[serde(default)]
    pub proactive: ProactiveConfig,
    #[serde(default)]
    pub initiative: InitiativeConfig,
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

#[derive(Deserialize, Clone)]
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

impl std::fmt::Debug for OpenRouterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterConfig")
            .field("url", &self.url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("classifier_model", &self.classifier_model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

fn default_openrouter_url() -> String { "https://openrouter.ai/api/v1".into() }

#[derive(Deserialize, Clone)]
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

impl std::fmt::Debug for ZenConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZenConfig")
            .field("url", &self.url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("classifier_model", &self.classifier_model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

fn default_zen_url() -> String { "https://opencode.ai/zen/v1".into() }

#[derive(Deserialize, Clone)]
pub struct TelegramConfig {
    /// Bot token (or set ATHENA_TELEGRAM_TOKEN env var)
    pub token: Option<String>,
    /// Allowed chat IDs (empty = deny all unless allow_all = true)
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// Allow all chats (must be explicitly set to true)
    #[serde(default)]
    pub allow_all: bool,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"[REDACTED]")
            .field("allowed_chats", &self.allowed_chats)
            .field("allow_all", &self.allow_all)
            .field("confirm_timeout_secs", &self.confirm_timeout_secs)
            .finish()
    }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            token: None,
            allowed_chats: vec![],
            allow_all: false,
            confirm_timeout_secs: default_confirm_timeout(),
        }
    }
}

fn default_confirm_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmbeddingConfig {
    #[serde(default = "default_embedding_enabled")]
    pub enabled: bool,
    #[serde(default = "default_model_dir")]
    pub model_dir: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: default_embedding_enabled(),
            model_dir: default_model_dir(),
        }
    }
}

fn default_embedding_enabled() -> bool { true }
fn default_model_dir() -> String { "~/.athena/models/all-MiniLM-L6-v2".into() }

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    #[serde(default = "default_half_life")]
    pub recency_half_life_days: f32,
    #[serde(default = "default_dedup_threshold")]
    pub dedup_threshold: f32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            recency_half_life_days: default_half_life(),
            dedup_threshold: default_dedup_threshold(),
        }
    }
}

fn default_half_life() -> f32 { 30.0 }
fn default_dedup_threshold() -> f32 { 0.95 }

#[derive(Debug, Deserialize, Clone)]
pub struct HeartbeatConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_heartbeat_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_heartbeat_jitter")]
    pub jitter: f64,
    pub soul_file: Option<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_heartbeat_interval(),
            jitter: default_heartbeat_jitter(),
            soul_file: None,
        }
    }
}

fn default_heartbeat_interval() -> u64 { 1800 }
fn default_heartbeat_jitter() -> f64 { 0.2 }

#[derive(Debug, Deserialize, Clone)]
pub struct MoodConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub timezone_offset: i32,
    #[serde(default = "default_mood_drift_interval")]
    pub drift_interval_secs: u64,
}

impl Default for MoodConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timezone_offset: 0,
            drift_interval_secs: default_mood_drift_interval(),
        }
    }
}

fn default_mood_drift_interval() -> u64 { 900 }

#[derive(Debug, Deserialize, Clone)]
pub struct ProactiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_memory_scan_interval")]
    pub memory_scan_interval_secs: u64,
    #[serde(default = "default_idle_threshold")]
    pub idle_threshold_secs: u64,
    #[serde(default = "default_spontaneity")]
    pub spontaneity: f32,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            memory_scan_interval_secs: default_memory_scan_interval(),
            idle_threshold_secs: default_idle_threshold(),
            spontaneity: default_spontaneity(),
        }
    }
}

fn default_memory_scan_interval() -> u64 { 3600 }
fn default_idle_threshold() -> u64 { 1800 }
fn default_spontaneity() -> f32 { 0.3 }

#[derive(Debug, Deserialize, Clone)]
pub struct InitiativeConfig {
    #[serde(default = "default_tolerance")]
    pub tolerance: f32,
    pub quiet_hours_start: Option<u32>,
    pub quiet_hours_end: Option<u32>,
}

impl Default for InitiativeConfig {
    fn default() -> Self {
        Self {
            tolerance: default_tolerance(),
            quiet_hours_start: None,
            quiet_hours_end: None,
        }
    }
}

fn default_tolerance() -> f32 { 0.5 }

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
pub struct GhostConfig {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Path to a soul file (markdown identity document)
    pub soul_file: Option<String>,
    /// Runtime-loaded soul content (not serialized)
    #[serde(skip)]
    pub soul: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PersonaConfig {
    /// Path to Athena's soul file
    pub soul_file: Option<String>,
    /// Runtime-loaded soul content (not serialized)
    #[serde(skip)]
    pub soul: Option<String>,
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
        r"unlink\s".into(),
        r"shred\s".into(),
        r"find\s.*-delete".into(),
        r"dd\s.*of=".into(),
        r"curl\s".into(),
        r"wget\s".into(),
        r"nc\s".into(),
        r"apt\s".into(),
        r"pip\s".into(),
        r"kill\s".into(),
        r"config\.toml".into(),
        r"\.env".into(),
        r"/etc/".into(),
        r"~/\.ssh".into(),
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
            ghosts: default_ghosts(),
            persona: PersonaConfig::default(),
            telegram: TelegramConfig::default(),
            embedding: EmbeddingConfig::default(),
            memory: MemoryConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            mood: MoodConfig::default(),
            proactive: ProactiveConfig::default(),
            initiative: InitiativeConfig::default(),
        }
    }
}

fn default_ghosts() -> Vec<GhostConfig> {
    vec![
        GhostConfig {
            name: "coder".into(),
            description: "Reads, writes, and modifies code files. Can run build/test commands.".into(),
            tools: vec!["file_read".into(), "file_write".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: false,
            }],
            strategy: "react".into(),
            soul_file: None,
            soul: None,
        },
        GhostConfig {
            name: "scout".into(),
            description: "Explores files and runs read-only commands. Cannot modify anything.".into(),
            tools: vec!["file_read".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: true,
            }],
            strategy: "react".into(),
            soul_file: None,
            soul: None,
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

        let mut config: Config = toml::from_str(&contents)
            .map_err(|e| AthenaError::Config(format!("Failed to parse config: {}", e)))?;

        config.validate();
        config.load_souls();
        Ok(config)
    }

    /// Load soul file contents for persona and all ghosts
    fn load_souls(&mut self) {
        if let Some(ref path) = self.persona.soul_file {
            match load_soul_file(path) {
                Ok(content) => {
                    tracing::info!("Loaded persona soul from {}", path);
                    self.persona.soul = Some(content);
                }
                Err(e) => tracing::warn!("Failed to load persona soul {}: {}", path, e),
            }
        }
        for ghost in &mut self.ghosts {
            if let Some(ref path) = ghost.soul_file {
                match load_soul_file(path) {
                    Ok(content) => {
                        tracing::info!("Loaded soul for ghost '{}' from {}", ghost.name, path);
                        ghost.soul = Some(content);
                    }
                    Err(e) => tracing::warn!("Failed to load soul for ghost '{}' from {}: {}", ghost.name, path, e),
                }
            }
        }
    }

    /// Warn about potentially insecure configuration
    fn validate(&self) {
        // M3: Warn on non-loopback HTTP URLs
        let url = &self.ollama.url;
        if url.starts_with("http://") {
            if let Some(host) = url.strip_prefix("http://").and_then(|s| s.split(':').next()) {
                if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                    tracing::warn!(
                        url = %url,
                        "Ollama URL uses unencrypted HTTP to a non-loopback address — traffic may be intercepted"
                    );
                }
            }
        }
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

    /// Resolve the embedding model directory, expanding ~ to home dir
    pub fn resolve_model_dir(&self) -> Result<PathBuf> {
        let path = if self.embedding.model_dir.starts_with("~/") {
            let home = dirs::home_dir()
                .ok_or_else(|| AthenaError::Config("Cannot find home directory".into()))?;
            home.join(&self.embedding.model_dir[2..])
        } else {
            PathBuf::from(&self.embedding.model_dir)
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

/// Resolve a path (expanding ~) and read the file contents.
fn load_soul_file(path: &str) -> std::result::Result<String, String> {
    let resolved = if path.starts_with("~/") {
        dirs::home_dir()
            .map(|h| h.join(&path[2..]))
            .ok_or_else(|| "Cannot find home directory".to_string())?
    } else {
        PathBuf::from(path)
    };
    std::fs::read_to_string(&resolved)
        .map_err(|e| format!("{}: {}", resolved.display(), e))
}
