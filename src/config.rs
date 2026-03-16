use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{SparksError, Result};
use crate::llm::{
    LlmProvider, OllamaClient, OpenAiClient, OpenAiCompatibleClient, OpenAiCompatibleConfig,
};
use crate::prompt_scanner::PromptScannerConfig;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub openai_api: OpenAiApiConfig,
    #[serde(default)]
    pub ollama: OllamaConfig,
    #[serde(default, alias = "ouath")]
    pub openai: Option<OpenAiConfig>,
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
    pub slack: SlackConfig,
    #[serde(default)]
    pub teams: TeamsConfig,
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
    pub ticket_intake: TicketIntakeConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub prompt_scanner: PromptScannerConfig,
    #[serde(default)]
    pub self_dev: SelfDevConfig,
    #[serde(default)]
    pub langfuse: LangfuseConfig,
    #[serde(default)]
    pub alerts: AlertsConfig,
    #[serde(default)]
    pub sonarqube: SonarqubeConfig,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
    #[serde(skip)]
    inline_secret_labels: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    #[serde(alias = "container_strict")]
    #[default]
    Standard,
    LocalOnly,
    SelfDevTrusted,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub profile: RuntimeProfile,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            profile: RuntimeProfile::Standard,
        }
    }
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
    "openai".into()
}

#[derive(Deserialize, Clone)]
pub struct OpenAiApiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_openai_api_bind")]
    pub bind: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_openai_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_openai_api_principal")]
    pub principal: String,
    #[serde(default = "default_openai_api_requests_per_minute")]
    pub requests_per_minute: u32,
    #[serde(default = "default_openai_api_burst")]
    pub burst: u32,
    #[serde(default)]
    pub advertised_models: Vec<String>,
}

impl std::fmt::Debug for OpenAiApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiApiConfig")
            .field("enabled", &self.enabled)
            .field("bind", &self.bind)
            .field("api_key", &"[REDACTED]")
            .field("api_key_env", &self.api_key_env)
            .field("principal", &self.principal)
            .field("requests_per_minute", &self.requests_per_minute)
            .field("burst", &self.burst)
            .field("advertised_models", &self.advertised_models)
            .finish()
    }
}

impl Default for OpenAiApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_openai_api_bind(),
            api_key: None,
            api_key_env: default_openai_api_key_env(),
            principal: default_openai_api_principal(),
            requests_per_minute: default_openai_api_requests_per_minute(),
            burst: default_openai_api_burst(),
            advertised_models: Vec::new(),
        }
    }
}

fn default_openai_api_bind() -> String {
    "127.0.0.1:8787".into()
}

fn default_openai_api_key_env() -> String {
    "SPARKS_OPENAI_API_KEY".into()
}

fn default_openai_api_principal() -> String {
    "self".into()
}

fn default_openai_api_requests_per_minute() -> u32 {
    120
}

fn default_openai_api_burst() -> u32 {
    30
}

#[derive(Deserialize, Clone)]
pub struct OpenAiConfig {
    #[serde(default = "default_openai_url")]
    pub url: String,
    /// Optional API style override: "responses" or "chat_completions".
    pub api_style: Option<String>,
    #[serde(default = "default_openai_model")]
    pub model: String,
    pub classifier_model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_openai_auth_file")]
    pub auth_file: String,
    /// Override the OAuth redirect URI (default: http://localhost:1455/auth/callback)
    pub redirect_uri: Option<String>,
    /// Optional reasoning effort for responses API (e.g., "low", "medium", "high")
    pub reasoning_effort: Option<String>,
    /// Optional reasoning summary behavior for responses API (e.g., "auto")
    pub reasoning_summary: Option<String>,
    /// Optional include fields for responses API (e.g., ["reasoning.encrypted_content"])
    #[serde(default)]
    pub include: Vec<String>,
}

impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("url", &self.url)
            .field("api_style", &self.api_style)
            .field("model", &self.model)
            .field("classifier_model", &self.classifier_model)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("context_window", &self.context_window)
            .field("auth_file", &self.auth_file)
            .field("redirect_uri", &self.redirect_uri)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("reasoning_summary", &self.reasoning_summary)
            .field("include", &self.include)
            .finish()
    }
}

fn default_openai_url() -> String {
    "https://chatgpt.com/backend-api/codex".into()
}

fn default_openai_model() -> String {
    "gpt-5.3-codex".into()
}

fn default_openai_auth_file() -> String {
    "~/.sparks/openai.json".into()
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            url: default_openai_url(),
            api_style: None,
            model: default_openai_model(),
            classifier_model: None,
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            context_window: default_context_window(),
            auth_file: default_openai_auth_file(),
            redirect_uri: None,
            reasoning_effort: None,
            reasoning_summary: None,
            include: Vec::new(),
        }
    }
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
    /// Bot token (or set SPARKS_TELEGRAM_TOKEN env var)
    pub token: Option<String>,
    /// Optional LLM provider override for Telegram (default: "openai")
    pub provider: Option<String>,
    /// Allowed chat IDs (empty = deny all unless allow_all = true)
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
    /// Allow all chats (must be explicitly set to true)
    #[serde(default)]
    pub allow_all: bool,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    /// Enable planning interview flow
    #[serde(default = "default_planning_enabled")]
    pub planning_enabled: bool,
    /// Auto-start planning interview when messages look like planning requests
    #[serde(default = "default_planning_auto")]
    pub planning_auto: bool,
    /// Planning interview timeout in seconds
    #[serde(default = "default_planning_timeout")]
    pub planning_timeout_secs: u64,
    /// Speech-to-text API URL (OpenAI Whisper-compatible endpoint)
    pub stt_url: Option<String>,
    /// STT API key (or set SPARKS_STT_API_KEY env var)
    pub stt_api_key: Option<String>,
    /// STT model name (default: whisper-large-v3)
    pub stt_model: Option<String>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"[REDACTED]")
            .field("provider", &self.provider)
            .field("allowed_chats", &self.allowed_chats)
            .field("allow_all", &self.allow_all)
            .field("confirm_timeout_secs", &self.confirm_timeout_secs)
            .field("planning_enabled", &self.planning_enabled)
            .field("planning_auto", &self.planning_auto)
            .field("planning_timeout_secs", &self.planning_timeout_secs)
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
            provider: None,
            allowed_chats: vec![],
            allow_all: false,
            confirm_timeout_secs: default_confirm_timeout(),
            planning_enabled: default_planning_enabled(),
            planning_auto: default_planning_auto(),
            planning_timeout_secs: default_planning_timeout(),
            stt_url: None,
            stt_api_key: None,
            stt_model: None,
        }
    }
}

// ── Slack config ─────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone)]
pub struct SlackConfig {
    /// Bot token (xoxb-...) or set SPARKS_SLACK_BOT_TOKEN env var
    pub bot_token: Option<String>,
    /// App-level token (xapp-...) for Socket Mode, or set SPARKS_SLACK_APP_TOKEN env var
    pub app_token: Option<String>,
    /// Signing secret for Events API verification, or set SPARKS_SLACK_SIGNING_SECRET env var
    pub signing_secret: Option<String>,
    /// Connection mode: "socket" (default) or "events_api"
    #[serde(default = "default_slack_mode")]
    pub mode: String,
    /// Bind address for Events API HTTP server (only used when mode = "events_api")
    #[serde(default = "default_slack_events_bind")]
    pub events_api_bind: String,
    /// Optional LLM provider override for Slack (default: "openai")
    pub provider: Option<String>,
    /// Allowed channel IDs (empty = deny all unless allow_all = true)
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    /// Allow all channels (must be explicitly set to true)
    #[serde(default)]
    pub allow_all: bool,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    /// Always reply in threads (default: true)
    #[serde(default = "default_slack_thread_replies")]
    pub thread_replies: bool,
    /// Enable planning interview flow
    #[serde(default = "default_planning_enabled")]
    pub planning_enabled: bool,
    /// Auto-start planning interview for planning-like messages
    #[serde(default = "default_planning_auto")]
    pub planning_auto: bool,
    /// Planning interview timeout in seconds
    #[serde(default = "default_planning_timeout")]
    pub planning_timeout_secs: u64,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &"[REDACTED]")
            .field("app_token", &"[REDACTED]")
            .field("signing_secret", &"[REDACTED]")
            .field("mode", &self.mode)
            .field("events_api_bind", &self.events_api_bind)
            .field("provider", &self.provider)
            .field("allowed_channels", &self.allowed_channels)
            .field("allow_all", &self.allow_all)
            .field("confirm_timeout_secs", &self.confirm_timeout_secs)
            .field("thread_replies", &self.thread_replies)
            .field("planning_enabled", &self.planning_enabled)
            .field("planning_auto", &self.planning_auto)
            .field("planning_timeout_secs", &self.planning_timeout_secs)
            .finish()
    }
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            bot_token: None,
            app_token: None,
            signing_secret: None,
            mode: default_slack_mode(),
            events_api_bind: default_slack_events_bind(),
            provider: None,
            allowed_channels: vec![],
            allow_all: false,
            confirm_timeout_secs: default_confirm_timeout(),
            thread_replies: default_slack_thread_replies(),
            planning_enabled: default_planning_enabled(),
            planning_auto: default_planning_auto(),
            planning_timeout_secs: default_planning_timeout(),
        }
    }
}

fn default_slack_mode() -> String {
    "socket".into()
}
fn default_slack_events_bind() -> String {
    "127.0.0.1:3000".into()
}
fn default_slack_thread_replies() -> bool {
    true
}

fn default_confirm_timeout() -> u64 {
    300
}

fn default_planning_enabled() -> bool {
    true
}

fn default_planning_auto() -> bool {
    true
}

fn default_planning_timeout() -> u64 {
    900
}

// ── Teams config ──────────────────────────────────────────────────────

/// Microsoft Teams Bot Framework configuration.
#[derive(Deserialize, Serialize, Clone)]
pub struct TeamsConfig {
    /// Azure AD App ID (bot's application ID), or set SPARKS_TEAMS_APP_ID env var
    pub app_id: Option<String>,
    /// Azure AD App Password (client secret), or set SPARKS_TEAMS_APP_PASSWORD env var
    pub app_password: Option<String>,
    /// HTTP bind address for the Bot Framework webhook endpoint
    #[serde(default = "default_teams_bind")]
    pub bind_addr: String,
    /// Optional LLM provider override
    pub provider: Option<String>,
    /// Allowed Azure AD tenant IDs (empty = deny all unless allow_all_tenants = true)
    #[serde(default)]
    pub allowed_tenants: Vec<String>,
    /// Allow all tenants (must be explicitly set to true)
    #[serde(default)]
    pub allow_all_tenants: bool,
    /// Rate limit cooldown in seconds (per user+conversation)
    #[serde(default = "default_teams_rate_limit")]
    pub rate_limit_secs: u64,
    /// Confirmation timeout in seconds
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    /// Enable planning interview flow
    #[serde(default = "default_planning_enabled")]
    pub planning_enabled: bool,
    /// Auto-start planning interview for planning-like messages
    #[serde(default = "default_planning_auto")]
    pub planning_auto: bool,
    /// Planning interview timeout in seconds
    #[serde(default = "default_planning_timeout")]
    pub planning_timeout_secs: u64,
    /// Skip JWT signature verification (development only - NEVER use in production)
    #[serde(default)]
    pub skip_auth: bool,
}

impl std::fmt::Debug for TeamsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamsConfig")
            .field("app_id", &self.app_id.as_deref().map(|_| "[REDACTED]"))
            .field("app_password", &"[REDACTED]")
            .field("bind_addr", &self.bind_addr)
            .field("provider", &self.provider)
            .field("allowed_tenants", &self.allowed_tenants)
            .field("allow_all_tenants", &self.allow_all_tenants)
            .field("rate_limit_secs", &self.rate_limit_secs)
            .field("confirm_timeout_secs", &self.confirm_timeout_secs)
            .field("planning_enabled", &self.planning_enabled)
            .field("planning_auto", &self.planning_auto)
            .field("planning_timeout_secs", &self.planning_timeout_secs)
            .field("skip_auth", &self.skip_auth)
            .finish()
    }
}

impl Default for TeamsConfig {
    fn default() -> Self {
        Self {
            app_id: None,
            app_password: None,
            bind_addr: default_teams_bind(),
            provider: None,
            allowed_tenants: vec![],
            allow_all_tenants: false,
            rate_limit_secs: default_teams_rate_limit(),
            confirm_timeout_secs: default_confirm_timeout(),
            planning_enabled: default_planning_enabled(),
            planning_auto: default_planning_auto(),
            planning_timeout_secs: default_planning_timeout(),
            skip_auth: false,
        }
    }
}

fn default_teams_bind() -> String {
    "0.0.0.0:3979".into()
}

fn default_teams_rate_limit() -> u64 {
    5
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
    "~/.sparks/models/all-MiniLM-L6-v2".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    #[serde(default = "default_half_life")]
    pub recency_half_life_days: f32,
    #[serde(default = "default_dedup_threshold")]
    pub dedup_threshold: f32,
    #[serde(default = "default_retrieval_cache_capacity")]
    pub retrieval_cache_capacity: usize,
    #[serde(default = "default_hnsw_enabled")]
    pub hnsw_enabled: bool,
    #[serde(default = "default_hnsw_min_index_size")]
    pub hnsw_min_index_size: usize,
    #[serde(default = "default_hnsw_m")]
    pub hnsw_m: usize,
    #[serde(default = "default_hnsw_ef_construction")]
    pub hnsw_ef_construction: usize,
    #[serde(default = "default_hnsw_ef_search")]
    pub hnsw_ef_search: usize,
    /// Half-life for exponential decay scoring expressed as f64 for higher precision.
    /// 0 = no decay. Default: 30 days.
    #[serde(default = "default_decay_half_life_days")]
    pub decay_half_life_days: f64,
    /// Soft cap on total active memories (0 = unlimited). Default: 10000.
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            recency_half_life_days: default_half_life(),
            dedup_threshold: default_dedup_threshold(),
            retrieval_cache_capacity: default_retrieval_cache_capacity(),
            hnsw_enabled: default_hnsw_enabled(),
            hnsw_min_index_size: default_hnsw_min_index_size(),
            hnsw_m: default_hnsw_m(),
            hnsw_ef_construction: default_hnsw_ef_construction(),
            hnsw_ef_search: default_hnsw_ef_search(),
            decay_half_life_days: default_decay_half_life_days(),
            max_entries: default_max_entries(),
        }
    }
}

fn default_half_life() -> f32 {
    30.0
}
fn default_dedup_threshold() -> f32 {
    0.95
}
fn default_retrieval_cache_capacity() -> usize {
    256
}
fn default_hnsw_enabled() -> bool {
    true
}
fn default_hnsw_min_index_size() -> usize {
    64
}
fn default_hnsw_m() -> usize {
    16
}
fn default_hnsw_ef_construction() -> usize {
    200
}
fn default_hnsw_ef_search() -> usize {
    64
}
fn default_decay_half_life_days() -> f64 {
    30.0
}
fn default_max_entries() -> usize {
    10000
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

// ── Alerts config ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AlertsConfig {
    /// Enable the alerting engine (default: true)
    #[serde(default = "default_alerts_enabled")]
    pub enabled: bool,
    /// How often to evaluate alert rules, in seconds (default: 30)
    #[serde(default = "default_alerts_interval")]
    pub check_interval_secs: u64,
    /// Default delivery channel: "log", "slack", "teams", "webhook" (default: "log")
    #[serde(default = "default_alerts_channel")]
    pub delivery_channel: String,
    /// Webhook URL for delivery_channel = "webhook"
    pub webhook_url: Option<String>,
    /// Minimum severity to deliver: "info", "warning", "critical" (default: "info")
    #[serde(default = "default_alerts_min_severity")]
    pub min_severity: String,
    /// Silence repeated alerts for this many seconds (default: 300)
    #[serde(default = "default_alerts_silence")]
    pub silence_secs: u64,
}

fn default_alerts_enabled() -> bool { true }
fn default_alerts_interval() -> u64 { 30 }
fn default_alerts_channel() -> String { "log".into() }
fn default_alerts_min_severity() -> String { "info".into() }
fn default_alerts_silence() -> u64 { 300 }

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: default_alerts_enabled(),
            check_interval_secs: default_alerts_interval(),
            delivery_channel: default_alerts_channel(),
            webhook_url: None,
            min_severity: default_alerts_min_severity(),
            silence_secs: default_alerts_silence(),
        }
    }
}

// ── SonarQube config ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct SonarqubeConfig {
    /// SonarQube server URL (e.g. https://sonarcloud.io or http://localhost:9000)
    #[serde(default = "default_sonar_url")]
    pub server_url: String,
    /// Authentication token (or set SPARKS_SONAR_TOKEN env var)
    pub token: Option<String>,
    /// Project key (e.g. "myorg_myproject")
    pub project_key: Option<String>,
    /// Organisation key (required for SonarCloud, omit for self-hosted)
    pub organization: Option<String>,
    /// Quality gate timeout in seconds (default 120)
    #[serde(default = "default_sonar_timeout")]
    pub gate_timeout_secs: u64,
    /// Poll interval in seconds (default 5)
    #[serde(default = "default_sonar_poll")]
    pub poll_interval_secs: u64,
    /// Whether to block PR creation on quality gate failure (default true)
    #[serde(default = "default_sonar_block")]
    pub block_on_failure: bool,
}

fn default_sonar_url() -> String {
    "https://sonarcloud.io".into()
}
fn default_sonar_timeout() -> u64 {
    120
}
fn default_sonar_poll() -> u64 {
    5
}
fn default_sonar_block() -> bool {
    true
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
    /// Trusted repo names allowed to run in host execution mode when
    /// runtime.profile = "self_dev_trusted".
    #[serde(default)]
    pub trusted_repos: Vec<String>,
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
            trusted_repos: Vec::new(),
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

// ── Snapshot config ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct SnapshotConfig {
    /// Enable automatic workspace snapshots (default: false - opt-in)
    #[serde(default)]
    pub enabled: bool,
    /// Directory to store snapshots (default: ~/.sparks/snapshots)
    pub snapshot_dir: Option<String>,
    /// Maximum number of snapshots to retain (default: 20, 0 = unlimited)
    #[serde(default = "default_snapshot_max")]
    pub max_snapshots: usize,
    /// Maximum workspace size in MB to snapshot (default: 50, 0 = no limit)
    #[serde(default = "default_snapshot_max_mb")]
    pub max_workspace_mb: u64,
    /// Glob patterns to include (default: ["."])
    #[serde(default = "default_snapshot_include")]
    pub include: Vec<String>,
    /// Glob patterns to exclude (default: ["target/", ".git/", "*.db"])
    #[serde(default = "default_snapshot_exclude")]
    pub exclude: Vec<String>,
}

fn default_snapshot_max() -> usize { 20 }
fn default_snapshot_max_mb() -> u64 { 50 }
fn default_snapshot_include() -> Vec<String> { vec![".".into()] }
fn default_snapshot_exclude() -> Vec<String> {
    vec!["target/".into(), ".git/".into(), ".worktrees/".into(), "*.db".into(), "*.log".into()]
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            snapshot_dir: None,
            max_snapshots: default_snapshot_max(),
            max_workspace_mb: default_snapshot_max_mb(),
            include: default_snapshot_include(),
            exclude: default_snapshot_exclude(),
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
pub struct TicketIntakeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ticket_intake_interval")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub mock_dispatch: bool,
    #[serde(default)]
    pub sources: Vec<TicketIntakeSourceConfig>,
    #[serde(default)]
    pub webhook: TicketIntakeWebhookConfig,
    #[serde(default)]
    pub ci_autopilot: TicketIntakeCiAutopilotConfig,
}

impl Default for TicketIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: default_ticket_intake_interval(),
            mock_dispatch: false,
            sources: Vec::new(),
            webhook: TicketIntakeWebhookConfig::default(),
            ci_autopilot: TicketIntakeCiAutopilotConfig::default(),
        }
    }
}

fn default_ticket_intake_interval() -> u64 {
    300
}

fn default_ticket_ci_autopilot_enabled() -> bool {
    true
}
fn default_ticket_ci_auto_merge() -> bool {
    true
}
fn default_ticket_ci_heal() -> bool {
    true
}
fn default_ticket_ci_max_heal_attempts() -> u8 {
    2
}
fn default_ticket_ci_poll_interval_secs() -> u64 {
    45
}
fn default_ticket_ci_timeout_secs() -> u64 {
    1200
}

fn default_ticket_intake_label() -> Option<String> {
    Some("sparks".to_string())
}

fn default_ticket_intake_webhook_bind() -> String {
    "127.0.0.1:8765".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct TicketIntakeCiAutopilotConfig {
    #[serde(default = "default_ticket_ci_autopilot_enabled")]
    pub enabled: bool,
    #[serde(default = "default_ticket_ci_auto_merge")]
    pub auto_merge: bool,
    #[serde(default = "default_ticket_ci_heal")]
    pub heal: bool,
    #[serde(default = "default_ticket_ci_max_heal_attempts")]
    pub max_heal_attempts: u8,
    #[serde(default = "default_ticket_ci_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_ticket_ci_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for TicketIntakeCiAutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: default_ticket_ci_autopilot_enabled(),
            auto_merge: default_ticket_ci_auto_merge(),
            heal: default_ticket_ci_heal(),
            max_heal_attempts: default_ticket_ci_max_heal_attempts(),
            poll_interval_secs: default_ticket_ci_poll_interval_secs(),
            timeout_secs: default_ticket_ci_timeout_secs(),
        }
    }
}

// Fields are read by the webhook feature (ticket_intake/webhook.rs)
#[cfg_attr(not(feature = "webhook"), allow(dead_code))]
#[derive(Debug, Deserialize, Clone)]
pub struct TicketIntakeWebhookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ticket_intake_webhook_bind")]
    pub bind: String,
    #[serde(default)]
    pub github_secret_env: Option<String>,
    #[serde(default)]
    pub gitlab_secret_env: Option<String>,
    #[serde(default)]
    pub linear_secret_env: Option<String>,
    #[serde(default)]
    pub jira_secret_env: Option<String>,
}

impl Default for TicketIntakeWebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_ticket_intake_webhook_bind(),
            github_secret_env: None,
            gitlab_secret_env: None,
            linear_secret_env: None,
            jira_secret_env: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TicketIntakeSourceConfig {
    pub provider: String,
    pub repo: String,
    #[serde(default = "default_ticket_intake_label")]
    pub filter_label: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub email_env: Option<String>,
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
    #[serde(default)]
    pub loop_guard: LoopGuardConfig,
    /// Directory containing dynamic tool YAML definitions (default: ~/.sparks/dynamic_tools/)
    pub dynamic_tools_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoopGuardConfig {
    #[serde(default = "default_loop_guard_enabled")]
    pub enabled: bool,
    #[serde(default = "default_loop_guard_window_size")]
    pub window_size: usize,
    #[serde(default = "default_loop_guard_repeat_threshold")]
    pub repeat_threshold: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mcp_discovery_ttl_secs")]
    pub discovery_ttl_secs: u64,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default = "default_mcp_server_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub transport: McpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_mcp_reconnect_delay_secs")]
    pub reconnect_delay_secs: u64,
    #[serde(default)]
    pub requires_confirmation: bool,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    #[default]
    Stdio,
    Sse,
    Websocket,
}

impl McpTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::Websocket => "websocket",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct GhostConfig {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
    #[serde(default)]
    pub role: GhostRole,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Path to a soul file (markdown identity document)
    pub soul_file: Option<String>,
    /// Path to a procedural skill file (markdown playbook/heuristics)
    pub skill_file: Option<String>,
    /// Additional procedural skill files merged in order.
    #[serde(default)]
    pub skill_files: Vec<String>,
    /// Runtime-loaded soul content (not serialized)
    #[serde(skip)]
    pub soul: Option<String>,
    /// Runtime-loaded skill content (not serialized)
    #[serde(skip)]
    pub skill: Option<String>,
    /// Docker image override (uses global docker.image if None)
    pub image: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GhostRole {
    Coordinator,
    Coder,
    Critic,
    #[default]
    Worker,
}

impl GhostRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Coder => "coder",
            Self::Critic => "critic",
            Self::Worker => "worker",
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PersonaConfig {
    /// Path to Sparks's soul file
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
    "rust:1.93".into()
}
fn default_socket_path() -> String {
    "/var/run/docker.sock".into()
}
fn default_memory_limit() -> i64 {
    268_435_456
} // 256MB
fn default_cpu_quota() -> i64 {
    50_000
}
fn default_timeout_secs() -> u64 {
    600
}
fn default_db_path() -> String {
    "~/.sparks/sparks.db".into()
}
fn default_max_steps() -> usize {
    15
}
fn default_loop_guard_enabled() -> bool {
    true
}
fn default_loop_guard_window_size() -> usize {
    12
}
fn default_loop_guard_repeat_threshold() -> usize {
    2
}
fn default_mcp_discovery_ttl_secs() -> u64 {
    60
}
fn default_mcp_server_enabled() -> bool {
    true
}
fn default_mcp_timeout_secs() -> u64 {
    30
}
fn default_mcp_reconnect_delay_secs() -> u64 {
    5
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
            loop_guard: LoopGuardConfig::default(),
            dynamic_tools_path: None,
        }
    }
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            enabled: default_loop_guard_enabled(),
            window_size: default_loop_guard_window_size(),
            repeat_threshold: default_loop_guard_repeat_threshold(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discovery_ttl_secs: default_mcp_discovery_ttl_secs(),
            servers: Vec::new(),
        }
    }
}

impl ManagerConfig {
    /// Resolve the dynamic tools directory, expanding ~ to home dir.
    /// Falls back to ~/.sparks/dynamic_tools/ if not configured.
    pub fn resolve_dynamic_tools_path(&self) -> Option<std::path::PathBuf> {
        let raw = self
            .dynamic_tools_path
            .clone()
            .unwrap_or_else(|| "~/.sparks/dynamic_tools".into());

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
            runtime: RuntimeConfig::default(),
            llm: LlmConfig::default(),
            openai_api: OpenAiApiConfig::default(),
            ollama: OllamaConfig::default(),
            openai: Some(OpenAiConfig::default()),
            openrouter: None,
            zen: None,
            docker: DockerConfig::default(),
            db: DbConfig::default(),
            manager: ManagerConfig::default(),
            ghosts: default_ghosts(),
            persona: PersonaConfig::default(),
            telegram: TelegramConfig::default(),
            slack: SlackConfig::default(),
            teams: TeamsConfig::default(),
            embedding: EmbeddingConfig::default(),
            memory: MemoryConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            mood: MoodConfig::default(),
            proactive: ProactiveConfig::default(),
            initiative: InitiativeConfig::default(),
            github: GithubConfig::default(),
            ticket_intake: TicketIntakeConfig::default(),
            mcp: McpConfig::default(),
            prompt_scanner: PromptScannerConfig::default(),
            self_dev: SelfDevConfig::default(),
            langfuse: LangfuseConfig::default(),
            alerts: AlertsConfig::default(),
            sonarqube: SonarqubeConfig::default(),
            snapshot: SnapshotConfig::default(),
            inline_secret_labels: Vec::new(),
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
            role: GhostRole::Coder,
            strategy: "react".into(),
            soul_file: None,
            skill_file: None,
            skill_files: Vec::new(),
            soul: None,
            skill: None,
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
            role: GhostRole::Worker,
            strategy: "react".into(),
            soul_file: None,
            skill_file: None,
            skill_files: Vec::new(),
            soul: None,
            skill: None,
            image: None,
        },
    ]
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = if let Some(p) = path {
            Some(p.to_path_buf())
        } else {
            // Look in current directory, then ~/.sparks/
            let local = PathBuf::from("config.toml");
            if local.exists() {
                Some(local)
            } else {
                let home = dirs::home_dir()
                    .ok_or_else(|| SparksError::Config("Cannot find home directory".into()))?;
                let global = home.join(".sparks").join("config.toml");
                if global.exists() {
                    Some(global)
                } else {
                    tracing::info!("No config file found, using defaults");
                    None
                }
            }
        };

        let mut config = if let Some(path) = config_path {
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                SparksError::Config(format!("Failed to read {}: {}", path.display(), e))
            })?;
            toml::from_str(&contents)
                .map_err(|e| SparksError::Config(format!("Failed to parse config: {}", e)))?
        } else {
            Config::default()
        };
        if config.ghosts.is_empty() {
            tracing::warn!(
                "No ghosts configured in config; applying built-in defaults (coder, scout)"
            );
            config.ghosts = default_ghosts();
        }

        config.inline_secret_labels = config.collect_inline_secret_labels();
        if !config.inline_secret_labels.is_empty() && !allow_inline_secrets() {
            return Err(SparksError::Config(format!(
                "Inline secrets found in config: {}. Move these to env vars or a .env file (gitignored). Set SPARKS_ALLOW_INLINE_SECRETS=1 to override.",
                config.inline_secret_labels.join(", ")
            )));
        }
        if !config.inline_secret_labels.is_empty() {
            tracing::warn!(
                fields = %config.inline_secret_labels.join(", "),
                "Inline secrets found in config; prefer environment variables or a local secret manager"
            );
        }

        config.apply_env_overrides();
        config.validate();
        config.load_souls();
        Ok(config)
    }

    pub fn inline_secret_labels(&self) -> &[String] {
        &self.inline_secret_labels
    }

    fn collect_inline_secret_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if self.github.token.is_some() && std::env::var("GH_TOKEN").is_err() {
            labels.push("github.token".to_string());
        }
        if self.openai_api.api_key.is_some() && std::env::var(&self.openai_api.api_key_env).is_err()
        {
            labels.push("openai_api.api_key".to_string());
        }
        if self.telegram.token.is_some() && std::env::var("SPARKS_TELEGRAM_TOKEN").is_err() {
            labels.push("telegram.token".to_string());
        }
        if self.telegram.stt_api_key.is_some() && std::env::var("SPARKS_STT_API_KEY").is_err() {
            labels.push("telegram.stt_api_key".to_string());
        }
        if self
            .openrouter
            .as_ref()
            .and_then(|c| c.api_key.as_ref())
            .is_some()
            && std::env::var("OPENROUTER_API_KEY").is_err()
        {
            labels.push("openrouter.api_key".to_string());
        }
        if self.zen.as_ref().and_then(|c| c.api_key.as_ref()).is_some()
            && std::env::var("OPENCODE_API_KEY").is_err()
        {
            labels.push("zen.api_key".to_string());
        }
        if self.langfuse.public_key.is_some() && std::env::var("LANGFUSE_PUBLIC_KEY").is_err() {
            labels.push("langfuse.public_key".to_string());
        }
        if self.langfuse.secret_key.is_some() && std::env::var("LANGFUSE_SECRET_KEY").is_err() {
            labels.push("langfuse.secret_key".to_string());
        }
        if self.slack.bot_token.is_some() && std::env::var("SPARKS_SLACK_BOT_TOKEN").is_err() {
            labels.push("slack.bot_token".to_string());
        }
        if self.slack.app_token.is_some() && std::env::var("SPARKS_SLACK_APP_TOKEN").is_err() {
            labels.push("slack.app_token".to_string());
        }
        if self.slack.signing_secret.is_some()
            && std::env::var("SPARKS_SLACK_SIGNING_SECRET").is_err()
        {
            labels.push("slack.signing_secret".to_string());
        }
        labels
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(token) = std::env::var("SPARKS_TELEGRAM_TOKEN") {
            self.telegram.token = Some(token);
        }
        if let Ok(key) = std::env::var("SPARKS_STT_API_KEY") {
            self.telegram.stt_api_key = Some(key);
        }
        if let Ok(token) = std::env::var("GH_TOKEN") {
            self.github.token = Some(token);
        }
        if let Some(openrouter) = self.openrouter.as_mut() {
            if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
                openrouter.api_key = Some(key);
            }
        }
        if let Some(zen) = self.zen.as_mut() {
            if let Ok(key) = std::env::var("OPENCODE_API_KEY") {
                zen.api_key = Some(key);
            }
        }
        if let Ok(key) = std::env::var("LANGFUSE_PUBLIC_KEY") {
            self.langfuse.public_key = Some(key);
        }
        if let Ok(key) = std::env::var("LANGFUSE_SECRET_KEY") {
            self.langfuse.secret_key = Some(key);
        }
        if let Ok(url) = std::env::var("LANGFUSE_BASE_URL") {
            self.langfuse.base_url = Some(url);
        }
        if let Ok(token) = std::env::var("SPARKS_SLACK_BOT_TOKEN") {
            self.slack.bot_token = Some(token);
        }
        if let Ok(token) = std::env::var("SPARKS_SLACK_APP_TOKEN") {
            self.slack.app_token = Some(token);
        }
        if let Ok(secret) = std::env::var("SPARKS_SLACK_SIGNING_SECRET") {
            self.slack.signing_secret = Some(secret);
        }
        if self.langfuse.enabled == false
            && self.langfuse.public_key.is_some()
            && self.langfuse.secret_key.is_some()
        {
            self.langfuse.enabled = true;
        }
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
        let mut skill_cache: HashMap<String, Option<String>> = HashMap::new();
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
            let skill_paths =
                resolve_ghost_skill_paths(ghost.skill_file.as_ref(), &ghost.skill_files);
            if !skill_paths.is_empty() {
                let mut loaded_skills: Vec<(String, String)> = Vec::new();
                for path in skill_paths {
                    if let Some(cached) = skill_cache.get(&path) {
                        if let Some(content) = cached {
                            loaded_skills.push((path, content.clone()));
                        }
                        continue;
                    }
                    match load_soul_file(&path) {
                        Ok(content) => {
                            tracing::info!("Loaded skill for ghost '{}' from {}", ghost.name, path);
                            skill_cache.insert(path.clone(), Some(content.clone()));
                            loaded_skills.push((path, content));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to load skill for ghost '{}' from {}: {}",
                                ghost.name,
                                path,
                                e
                            );
                            skill_cache.insert(path, None);
                        }
                    }
                }
                if !loaded_skills.is_empty() {
                    ghost.skill = Some(compose_ghost_skill_bundle(&loaded_skills));
                }
            }
        }
    }

    /// Warn about potentially insecure configuration
    fn validate(&self) {
        // M3: Warn on non-loopback HTTP URLs
        let url = &self.ollama.url;
        if url.starts_with("http://") {
            if !self.ollama_url_is_loopback() {
                tracing::warn!(
                    url = %url,
                    "Ollama URL uses unencrypted HTTP to a non-loopback address — traffic may be intercepted"
                );
            }
        }
    }

    pub fn runtime_profile_name(&self) -> &'static str {
        match self.runtime.profile {
            RuntimeProfile::Standard => "container_strict",
            RuntimeProfile::LocalOnly => "local_only",
            RuntimeProfile::SelfDevTrusted => "self_dev_trusted",
        }
    }

    pub fn local_only_enabled(&self) -> bool {
        matches!(self.runtime.profile, RuntimeProfile::LocalOnly)
    }

    pub fn self_dev_trusted_enabled(&self) -> bool {
        matches!(self.runtime.profile, RuntimeProfile::SelfDevTrusted)
    }

    pub fn trusted_self_dev_repos(&self) -> Vec<String> {
        self.self_dev
            .trusted_repos
            .iter()
            .map(|repo| repo.trim().to_ascii_lowercase())
            .filter(|repo| !repo.is_empty())
            .collect()
    }

    pub fn ollama_url_host(&self) -> Option<String> {
        parse_url_host(&self.ollama.url)
    }

    pub fn ollama_url_is_loopback(&self) -> bool {
        self.ollama_url_host()
            .as_deref()
            .map(is_loopback_host)
            .unwrap_or(false)
    }

    /// Ordered provider candidates: configured provider first, then common fallbacks.
    pub fn provider_candidates(&self) -> Vec<String> {
        let mut out = vec![self.llm.provider.clone()];
        for p in ["openai", "ollama", "openrouter", "zen", "ouath"] {
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
            "openai" | "ouath" => {
                let cfg = self.openai.clone().unwrap_or_default();
                Ok(Arc::new(OpenAiClient::new(cfg)))
            }
            "openrouter" => {
                let cfg = self.openrouter.as_ref().ok_or_else(|| {
                    SparksError::Config(
                        "provider = \"openrouter\" but [openrouter] section is missing".into(),
                    )
                })?;
                let api_key = cfg
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                    .ok_or_else(|| {
                        SparksError::Config(
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
                    SparksError::Config("provider = \"zen\" but [zen] section is missing".into())
                })?;
                let api_key = cfg
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                    .ok_or_else(|| {
                        SparksError::Config(
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
            other => Err(SparksError::Config(format!(
                "Unknown LLM provider: {}",
                other
            ))),
        }
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
            "openai" | "ouath" => {
                let cfg = self.openai.clone().unwrap_or_default();
                match &cfg.classifier_model {
                    Some(model) => {
                        let mut cloned = cfg.clone();
                        cloned.model = model.clone();
                        Ok(Arc::new(OpenAiClient::new(cloned)))
                    }
                    None => Ok(fallback.clone()),
                }
            }
            "openrouter" => {
                let cfg = self.openrouter.as_ref().ok_or_else(|| {
                    SparksError::Config(
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
                                SparksError::Config(
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
                    SparksError::Config("provider = \"zen\" but [zen] section is missing".into())
                })?;
                match &cfg.classifier_model {
                    Some(model) => {
                        let api_key = cfg
                            .api_key
                            .clone()
                            .or_else(|| std::env::var("OPENCODE_API_KEY").ok())
                            .ok_or_else(|| {
                                SparksError::Config(
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

    /// Resolve the db path, expanding ~ to home dir
    pub fn db_path(&self) -> Result<PathBuf> {
        let path = if self.db.path.starts_with("~/") {
            let home = dirs::home_dir()
                .ok_or_else(|| SparksError::Config("Cannot find home directory".into()))?;
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
                .ok_or_else(|| SparksError::Config("Cannot find home directory".into()))?;
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

fn allow_inline_secrets() -> bool {
    matches!(
        std::env::var("SPARKS_ALLOW_INLINE_SECRETS")
            .as_deref()
            .unwrap_or(""),
        "1" | "true" | "TRUE" | "yes" | "YES"
    )
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

pub fn resolve_ghost_skill_paths(
    legacy_skill_file: Option<&String>,
    skill_files: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(path) = legacy_skill_file {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    for path in skill_files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

pub fn compose_ghost_skill_bundle(skills: &[(String, String)]) -> String {
    if skills.len() == 1 {
        return skills[0].1.clone();
    }
    skills
        .iter()
        .map(|(path, content)| format!("## SKILL: {}\n{}", path, content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parse_url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    (!host.is_empty()).then(|| host.to_string())
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{compose_ghost_skill_bundle, resolve_ghost_skill_paths, Config, RuntimeProfile};
    use std::path::Path;
    use std::sync::Mutex;

    // Serialize tests that mutate env vars to prevent parallel races.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_profile_name_maps_standard_to_container_strict() {
        let config = Config::default();
        assert_eq!(config.runtime_profile_name(), "container_strict");
    }

    #[test]
    fn trusted_repo_helpers_normalize_and_match() {
        let mut config = Config::default();
        config.runtime.profile = RuntimeProfile::SelfDevTrusted;
        config.self_dev.trusted_repos = vec![" Sparks ".to_string(), "".to_string()];
        assert_eq!(config.trusted_self_dev_repos(), vec!["sparks"]);
    }

    #[test]
    fn load_hydrates_default_ghosts_when_missing_in_file() {
        let path = temp_config_path("sparks-config-no-ghosts");
        std::fs::write(
            &path,
            r#"
[llm]
provider = "ollama"
"#,
        )
        .unwrap();

        let config = Config::load(Some(Path::new(&path))).unwrap();
        assert!(!config.ghosts.is_empty());
        assert!(config.ghosts.iter().any(|g| g.name == "coder"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_preserves_explicit_ghosts_from_file() {
        let path = temp_config_path("sparks-config-explicit-ghost");
        std::fs::write(
            &path,
            r#"
[llm]
provider = "ollama"

[[ghosts]]
name = "custom"
description = "custom ghost"
tools = ["shell"]
strategy = "react"
"#,
        )
        .unwrap();

        let config = Config::load(Some(Path::new(&path))).unwrap();
        assert_eq!(config.ghosts.len(), 1);
        assert_eq!(config.ghosts[0].name, "custom");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolve_ghost_skill_paths_merges_and_deduplicates() {
        let legacy = Some(&"skills/base.md".to_string());
        let paths = vec![
            "skills/base.md".to_string(),
            "skills/security.md".to_string(),
            " skills/security.md ".to_string(),
            "".to_string(),
        ];
        let resolved = resolve_ghost_skill_paths(legacy, &paths);
        assert_eq!(
            resolved,
            vec![
                "skills/base.md".to_string(),
                "skills/security.md".to_string()
            ]
        );
    }

    #[test]
    fn compose_ghost_skill_bundle_adds_headers_for_multi_skill() {
        let bundle = compose_ghost_skill_bundle(&[
            ("skills/a.md".to_string(), "alpha".to_string()),
            ("skills/b.md".to_string(), "beta".to_string()),
        ]);
        assert!(bundle.contains("## SKILL: skills/a.md"));
        assert!(bundle.contains("alpha"));
        assert!(bundle.contains("## SKILL: skills/b.md"));
        assert!(bundle.contains("beta"));
    }

    #[test]
    fn apply_env_overrides_loads_slack_tokens_from_env() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("SPARKS_SLACK_BOT_TOKEN", "xoxb-test-bot");
        std::env::set_var("SPARKS_SLACK_APP_TOKEN", "xapp-test-app");
        std::env::set_var("SPARKS_SLACK_SIGNING_SECRET", "test-signing-secret");

        let mut config = Config::default();
        config.apply_env_overrides();

        assert_eq!(config.slack.bot_token.as_deref(), Some("xoxb-test-bot"));
        assert_eq!(config.slack.app_token.as_deref(), Some("xapp-test-app"));
        assert_eq!(
            config.slack.signing_secret.as_deref(),
            Some("test-signing-secret")
        );

        std::env::remove_var("SPARKS_SLACK_BOT_TOKEN");
        std::env::remove_var("SPARKS_SLACK_APP_TOKEN");
        std::env::remove_var("SPARKS_SLACK_SIGNING_SECRET");
    }

    #[test]
    fn collect_inline_secret_labels_flags_slack_tokens_not_in_env() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("SPARKS_SLACK_BOT_TOKEN");
        std::env::remove_var("SPARKS_SLACK_APP_TOKEN");
        std::env::remove_var("SPARKS_SLACK_SIGNING_SECRET");

        let mut config = Config::default();
        config.slack.bot_token = Some("xoxb-inline".to_string());
        config.slack.app_token = Some("xapp-inline".to_string());
        config.slack.signing_secret = Some("inline-secret".to_string());

        let labels = config.collect_inline_secret_labels();
        assert!(labels.contains(&"slack.bot_token".to_string()));
        assert!(labels.contains(&"slack.app_token".to_string()));
        assert!(labels.contains(&"slack.signing_secret".to_string()));
    }

    #[test]
    fn collect_inline_secret_labels_skips_slack_token_when_env_is_set() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("SPARKS_SLACK_BOT_TOKEN", "xoxb-from-env");
        std::env::remove_var("SPARKS_SLACK_APP_TOKEN");
        std::env::remove_var("SPARKS_SLACK_SIGNING_SECRET");

        let mut config = Config::default();
        config.slack.bot_token = Some("xoxb-inline".to_string());

        let labels = config.collect_inline_secret_labels();
        assert!(!labels.contains(&"slack.bot_token".to_string()));

        std::env::remove_var("SPARKS_SLACK_BOT_TOKEN");
    }

    fn temp_config_path(prefix: &str) -> String {
        std::env::temp_dir()
            .join(format!("{}-{}.toml", prefix, uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string()
    }
}
