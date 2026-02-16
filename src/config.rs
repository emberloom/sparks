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
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub self_dev: SelfDevConfig,
    #[serde(default)]
    pub langfuse: LangfuseConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
        }
    }
}

fn default_provider() -> String {
    "ollama".into()
}

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
    #[serde(default = "default_context_window")]
    pub context_window: u64,
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
            .field("context_window", &self.context_window)
            .finish()
    }
}

fn default_openrouter_url() -> String {
    "https://openrouter.ai/api/v1".into()
}
fn default_context_window() -> u64 {
    128_000
}

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
    #[serde(default = "default_context_window")]
    pub context_window: u64,
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
            .field("context_window", &self.context_window)
            .finish()
    }
}

fn default_zen_url() -> String {
    "https://opencode.ai/zen/v1".into()
}

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
    /// Speech-to-text API URL (OpenAI Whisper-compatible endpoint)
    pub stt_url: Option<String>,
    /// STT API key (or set ATHENA_STT_API_KEY env var)
    pub stt_api_key: Option<String>,
    /// STT model name (default: whisper-large-v3)
    pub stt_model: Option<String>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"[REDACTED]")
            .field("allowed_chats", &self.allowed_chats)
            .field("allow_all", &self.allow_all)
            .field("confirm_timeout_secs", &self.confirm_timeout_secs)
            .field("stt_url", &self.stt_url)
            .field("stt_api_key", &"[REDACTED]")
            .field("stt_model", &self.stt_model)
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
            stt_url: None,
            stt_api_key: None,
            stt_model: None,
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

fn default_embedding_enabled() -> bool {
    true
}
fn default_model_dir() -> String {
    "~/.athena/models/all-MiniLM-L6-v2".into()
}

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

fn default_half_life() -> f32 {
    30.0
}
fn default_dedup_threshold() -> f32 {
    0.95
}

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

fn default_heartbeat_interval() -> u64 {
    1800
}
fn default_heartbeat_jitter() -> f64 {
    0.2
}

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

fn default_mood_drift_interval() -> u64 {
    900
}

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
    #[serde(default = "default_reentry_delay")]
    pub reentry_delay_secs: u64,
    #[serde(default = "default_reentry_jitter")]
    pub reentry_jitter: f64,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            memory_scan_interval_secs: default_memory_scan_interval(),
            idle_threshold_secs: default_idle_threshold(),
            spontaneity: default_spontaneity(),
            reentry_delay_secs: default_reentry_delay(),
            reentry_jitter: default_reentry_jitter(),
        }
    }
}

fn default_memory_scan_interval() -> u64 {
    3600
}
fn default_idle_threshold() -> u64 {
    1800
}
fn default_spontaneity() -> f32 {
    0.3
}
fn default_reentry_delay() -> u64 {
    7200
} // 2 hours
fn default_reentry_jitter() -> f64 {
    0.7
} // ±70% → 36min to 3h24m

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

fn default_tolerance() -> f32 {
    0.5
}

#[derive(Debug, Deserialize, Clone)]
pub struct SelfDevConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_metrics_interval")]
    pub metrics_interval_secs: u64,
    #[serde(default)]
    pub code_indexer_enabled: bool,
    #[serde(default = "default_code_indexer_interval")]
    pub code_indexer_interval_secs: u64,
    #[serde(default)]
    pub refactoring_scan_enabled: bool,
    #[serde(default = "default_refactoring_scan_interval")]
    pub refactoring_scan_interval_secs: u64,
}

impl Default for SelfDevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            metrics_interval_secs: default_metrics_interval(),
            code_indexer_enabled: false,
            code_indexer_interval_secs: default_code_indexer_interval(),
            refactoring_scan_enabled: false,
            refactoring_scan_interval_secs: default_refactoring_scan_interval(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LangfuseConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

impl Default for LangfuseConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("LANGFUSE_PUBLIC_KEY").is_ok(),
            public_key: std::env::var("LANGFUSE_PUBLIC_KEY").ok(),
            secret_key: std::env::var("LANGFUSE_SECRET_KEY").ok(),
            base_url: std::env::var("LANGFUSE_BASE_URL").ok(),
        }
    }
}

fn default_metrics_interval() -> u64 {
    30
}
fn default_code_indexer_interval() -> u64 {
    14400
} // 4 hours
fn default_refactoring_scan_interval() -> u64 {
    21600
} // 6 hours

#[derive(Deserialize, Clone)]
pub struct GithubConfig {
    pub token: Option<String>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self { token: None }
    }
}

impl std::fmt::Debug for GithubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
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
    /// Directory containing dynamic tool YAML definitions (default: ~/.athena/dynamic_tools/)
    pub dynamic_tools_path: Option<String>,
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
    /// Docker image override (uses global docker.image if None)
    pub image: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PersonaConfig {
    /// Path to Athena's soul file
    pub soul_file: Option<String>,
    /// Runtime-loaded soul content (not serialized)
    #[serde(skip)]
    pub soul: Option<String>,
    /// Path to technical self-knowledge document
    pub self_file: Option<String>,
    /// Runtime-loaded self-knowledge content (not serialized)
    #[serde(skip)]
    pub self_knowledge: Option<String>,
    /// Path to tool reference document (injected into ghost system prompts)
    pub tools_file: Option<String>,
    /// Runtime-loaded tools document content (not serialized)
    #[serde(skip)]
    pub tools_doc: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MountConfig {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

// Defaults
fn default_ollama_url() -> String {
    "http://localhost:11434".into()
}
fn default_model() -> String {
    "llama3.2".into()
}
fn default_temperature() -> f32 {
    0.3
}
fn default_max_tokens() -> u32 {
    4096
}
fn default_image() -> String {
    "rust:1.84-slim".into()
}
fn default_socket_path() -> String {
    "/var/run/docker.sock".into()
}
fn default_runtime() -> String {
    "runc".into()
}
fn default_memory_limit() -> i64 {
    268_435_456
} // 256MB
fn default_cpu_quota() -> i64 {
    50_000
}
fn default_timeout_secs() -> u64 {
    120
}
fn default_db_path() -> String {
    "~/.athena/athena.db".into()
}
fn default_max_steps() -> usize {
    15
}
fn default_strategy() -> String {
    "react".into()
}
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
        Self {
            path: default_db_path(),
        }
    }
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            sensitive_patterns: default_sensitive_patterns(),
            dynamic_tools_path: None,
        }
    }
}

impl ManagerConfig {
    /// Resolve the dynamic tools directory, expanding ~ to home dir.
    /// Falls back to ~/.athena/dynamic_tools/ if not configured.
    pub fn resolve_dynamic_tools_path(&self) -> Option<std::path::PathBuf> {
        let raw = self
            .dynamic_tools_path
            .clone()
            .unwrap_or_else(|| "~/.athena/dynamic_tools".into());

        let path = if raw.starts_with("~/") {
            dirs::home_dir().map(|h| h.join(&raw[2..]))?
        } else {
            std::path::PathBuf::from(&raw)
        };

        Some(path)
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
            github: GithubConfig::default(),
            self_dev: SelfDevConfig::default(),
            langfuse: LangfuseConfig::default(),
        }
    }
}

fn default_ghosts() -> Vec<GhostConfig> {
    vec![
        GhostConfig {
            name: "coder".into(),
            description: "Reads, writes, and modifies code files. Can run build/test commands."
                .into(),
            tools: vec!["file_read".into(), "file_write".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: false,
            }],
            strategy: "react".into(),
            soul_file: None,
            soul: None,
            image: None,
        },
        GhostConfig {
            name: "scout".into(),
            description: "Explores files and runs read-only commands. Cannot modify anything."
                .into(),
            tools: vec!["file_read".into(), "shell".into()],
            mounts: vec![MountConfig {
                host_path: ".".into(),
                container_path: "/workspace".into(),
                read_only: true,
            }],
            strategy: "react".into(),
            soul_file: None,
            soul: None,
            image: None,
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

        let contents = std::fs::read_to_string(&config_path).map_err(|e| {
            AthenaError::Config(format!("Failed to read {}: {}", config_path.display(), e))
        })?;

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
        if let Some(ref path) = self.persona.self_file {
            match load_soul_file(path) {
                Ok(content) => {
                    tracing::info!("Loaded self-knowledge from {}", path);
                    self.persona.self_knowledge = Some(content);
                }
                Err(e) => tracing::warn!("Failed to load self-knowledge {}: {}", path, e),
            }
        }
        if let Some(ref path) = self.persona.tools_file {
            match load_soul_file(path) {
                Ok(content) => {
                    tracing::info!("Loaded tools reference from {}", path);
                    self.persona.tools_doc = Some(content);
                }
                Err(e) => tracing::warn!("Failed to load tools reference {}: {}", path, e),
            }
        }
        for ghost in &mut self.ghosts {
            if let Some(ref path) = ghost.soul_file {
                match load_soul_file(path) {
                    Ok(content) => {
                        tracing::info!("Loaded soul for ghost '{}' from {}", ghost.name, path);
                        ghost.soul = Some(content);
                    }
                    Err(e) => tracing::warn!(
                        "Failed to load soul for ghost '{}' from {}: {}",
                        ghost.name,
                        path,
                        e
                    ),
                }
            }
        }
    }

    /// Warn about potentially insecure configuration
    fn validate(&self) {
        // M3: Warn on non-loopback HTTP URLs
        let url = &self.ollama.url;
        if url.starts_with("http://") {
            if let Some(host) = url
                .strip_prefix("http://")
                .and_then(|s| s.split(':').next())
            {
                if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                    tracing::warn!(
                        url = %url,
                        "Ollama URL uses unencrypted HTTP to a non-loopback address — traffic may be intercepted"
                    );
                }
            }
        }

        let mut inline_secrets = Vec::new();
        if self.github.token.is_some() {
            inline_secrets.push("github.token");
        }
        if self.telegram.token.is_some() {
            inline_secrets.push("telegram.token");
        }
        if self.telegram.stt_api_key.is_some() {
            inline_secrets.push("telegram.stt_api_key");
        }
        if self
            .openrouter
            .as_ref()
            .and_then(|c| c.api_key.as_ref())
            .is_some()
        {
            inline_secrets.push("openrouter.api_key");
        }
        if self.zen.as_ref().and_then(|c| c.api_key.as_ref()).is_some() {
            inline_secrets.push("zen.api_key");
        }
        if self.langfuse.public_key.is_some() {
            inline_secrets.push("langfuse.public_key");
        }
        if self.langfuse.secret_key.is_some() {
            inline_secrets.push("langfuse.secret_key");
        }
        if !inline_secrets.is_empty() {
            tracing::warn!(
                fields = %inline_secrets.join(", "),
                "Inline secrets found in config; prefer environment variables or a local secret manager"
            );
        }
    }

    /// Ordered provider candidates: configured provider first, then common fallbacks.
    pub fn provider_candidates(&self) -> Vec<String> {
        let mut out = vec![self.llm.provider.clone()];
        for p in ["openrouter", "zen", "ollama"] {
            if !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
        out
    }

    /// Build an LLM provider by explicit name.
    pub fn build_llm_provider_for(&self, provider: &str) -> Result<Arc<dyn LlmProvider>> {
        match provider {
            "ollama" => Ok(Arc::new(OllamaClient::new(self.ollama.clone()))),
            "openrouter" => {
                let cfg = self.openrouter.as_ref().ok_or_else(|| {
                    AthenaError::Config(
                        "provider = \"openrouter\" but [openrouter] section is missing".into(),
                    )
                })?;
                let api_key = cfg
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                    .ok_or_else(|| {
                        AthenaError::Config(
                            "OpenRouter API key not set (config api_key or OPENROUTER_API_KEY env)"
                                .into(),
                        )
                    })?;
                Ok(Arc::new(OpenAiCompatibleClient::new(
                    OpenAiCompatibleConfig {
                        url: cfg.url.clone(),
                        api_key,
                        model: cfg.model.clone(),
                        temperature: cfg.temperature,
                        max_tokens: cfg.max_tokens,
                        context_window: cfg.context_window,
                    },
                    "OpenRouter",
                )))
            }
            "zen" => {
                let cfg = self.zen.as_ref().ok_or_else(|| {
                    AthenaError::Config("provider = \"zen\" but [zen] section is missing".into())
                })?;
                let api_key = cfg
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                    .ok_or_else(|| {
                        AthenaError::Config(
                            "Opencode Zen API key not set (config api_key or OPENCODE_API_KEY env)"
                                .into(),
                        )
                    })?;
                Ok(Arc::new(OpenAiCompatibleClient::new(
                    OpenAiCompatibleConfig {
                        url: cfg.url.clone(),
                        api_key,
                        model: cfg.model.clone(),
                        temperature: cfg.temperature,
                        max_tokens: cfg.max_tokens,
                        context_window: cfg.context_window,
                    },
                    "Opencode Zen",
                )))
            }
            other => Err(AthenaError::Config(format!(
                "Unknown LLM provider: {}",
                other
            ))),
        }
    }

    /// Build the configured LLM provider
    pub fn build_llm_provider(&self) -> Result<Arc<dyn LlmProvider>> {
        self.build_llm_provider_for(self.llm.provider.as_str())
    }

    /// Build the orchestrator provider for an explicit provider name.
    pub fn build_orchestrator_provider_for(
        &self,
        provider: &str,
        fallback: &Arc<dyn LlmProvider>,
    ) -> Result<Arc<dyn LlmProvider>> {
        match provider {
            "ollama" => match &self.ollama.classifier_model {
                Some(model) => {
                    let mut cfg = self.ollama.clone();
                    cfg.model = model.clone();
                    Ok(Arc::new(OllamaClient::new(cfg)))
                }
                None => Ok(fallback.clone()),
            },
            "openrouter" => {
                let cfg = self.openrouter.as_ref().ok_or_else(|| {
                    AthenaError::Config(
                        "provider = \"openrouter\" but [openrouter] section is missing".into(),
                    )
                })?;
                match &cfg.classifier_model {
                    Some(model) => {
                        let api_key = cfg
                            .api_key
                            .clone()
                            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                            .ok_or_else(|| {
                                AthenaError::Config(
                                    "OpenRouter API key not set (config api_key or OPENROUTER_API_KEY env)"
                                        .into(),
                                )
                            })?;
                        Ok(Arc::new(OpenAiCompatibleClient::new(
                            OpenAiCompatibleConfig {
                                url: cfg.url.clone(),
                                api_key,
                                model: model.clone(),
                                temperature: cfg.temperature,
                                max_tokens: cfg.max_tokens,
                                context_window: cfg.context_window,
                            },
                            "OpenRouter/orchestrator",
                        )))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            "zen" => {
                let cfg = self.zen.as_ref().ok_or_else(|| {
                    AthenaError::Config("provider = \"zen\" but [zen] section is missing".into())
                })?;
                match &cfg.classifier_model {
                    Some(model) => {
                        let api_key = cfg
                            .api_key
                            .clone()
                            .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                            .ok_or_else(|| {
                                AthenaError::Config(
                                    "Opencode Zen API key not set (config api_key or OPENCODE_API_KEY env)"
                                        .into(),
                                )
                            })?;
                        Ok(Arc::new(OpenAiCompatibleClient::new(
                            OpenAiCompatibleConfig {
                                url: cfg.url.clone(),
                                api_key,
                                model: model.clone(),
                                temperature: cfg.temperature,
                                max_tokens: cfg.max_tokens,
                                context_window: cfg.context_window,
                            },
                            "Zen/orchestrator",
                        )))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            _ => Ok(fallback.clone()),
        }
    }

    /// Build the orchestrator LLM provider (falls back to main provider if no classifier_model set)
    pub fn build_orchestrator_provider(
        &self,
        fallback: &Arc<dyn LlmProvider>,
    ) -> Result<Arc<dyn LlmProvider>> {
        self.build_orchestrator_provider_for(self.llm.provider.as_str(), fallback)
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
pub fn load_soul_file(path: &str) -> std::result::Result<String, String> {
    let resolved = if path.starts_with("~/") {
        dirs::home_dir()
            .map(|h| h.join(&path[2..]))
            .ok_or_else(|| "Cannot find home directory".to_string())?
    } else {
        PathBuf::from(path)
    };
    std::fs::read_to_string(&resolved).map_err(|e| format!("{}: {}", resolved.display(), e))
}
