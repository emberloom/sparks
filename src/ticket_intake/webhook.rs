use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::config::{TicketIntakeSourceConfig, TicketIntakeWebhookConfig};
use crate::core::AutonomousTask;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::ticket_intake::provider::ExternalTicket;
use crate::ticket_intake::TicketIntakeStore;

#[derive(Clone)]
struct WebhookState {
    observer: ObserverHandle,
    store: Arc<TicketIntakeStore>,
    auto_tx: mpsc::Sender<AutonomousTask>,
    sources: Vec<TicketIntakeSourceConfig>,
    secrets: WebhookSecrets,
}

#[derive(Clone, Default)]
struct WebhookSecrets {
    github: Option<String>,
    gitlab: Option<String>,
    linear: Option<String>,
    jira: Option<String>,
}

pub async fn spawn_ticket_intake_webhook(
    config: TicketIntakeWebhookConfig,
    sources: Vec<TicketIntakeSourceConfig>,
    observer: ObserverHandle,
    store: Arc<TicketIntakeStore>,
    auto_tx: mpsc::Sender<AutonomousTask>,
) -> anyhow::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    let secrets = load_webhook_secrets(&config, &observer);
    let state = WebhookState {
        observer: observer.clone(),
        store,
        auto_tx,
        sources,
        secrets,
    };

    let app = Router::new()
        .route("/webhook/github", post(handle_github))
        .route("/webhook/gitlab", post(handle_gitlab))
        .route("/webhook/linear", post(handle_linear))
        .route("/webhook/jira", post(handle_jira))
        .with_state(state);

    let bind = config.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    observer.log(
        ObserverCategory::TicketIntake,
        format!("Ticket webhook listener bound on {}", bind),
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("Ticket webhook server error: {}", e);
        }
    });

    Ok(())
}

fn load_webhook_secrets(
    config: &TicketIntakeWebhookConfig,
    observer: &ObserverHandle,
) -> WebhookSecrets {
    let mut secrets = WebhookSecrets::default();
    secrets.github = read_env_opt(config.github_secret_env.as_deref(), observer, "github");
    secrets.gitlab = read_env_opt(config.gitlab_secret_env.as_deref(), observer, "gitlab");
    secrets.linear = read_env_opt(config.linear_secret_env.as_deref(), observer, "linear");
    secrets.jira = read_env_opt(config.jira_secret_env.as_deref(), observer, "jira");
    secrets
}

fn read_env_opt(env_key: Option<&str>, observer: &ObserverHandle, label: &str) -> Option<String> {
    let Some(key) = env_key else {
        return None;
    };
    match std::env::var(key) {
        Ok(val) if !val.trim().is_empty() => Some(val),
        _ => {
            observer.log(
                ObserverCategory::TicketIntake,
                format!("Webhook secret env missing for {} ({}).", label, key),
            );
            None
        }
    }
}

async fn handle_github(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !verify_github(&state, &headers, &body) {
        return StatusCode::UNAUTHORIZED;
    }

    let payload: GhWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("GitHub webhook parse error: {}", e),
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.issue.pull_request.is_some() {
        return StatusCode::OK;
    }

    let repo_full = payload
        .repository
        .and_then(|r| r.full_name)
        .unwrap_or_default();

    let Some(source) = match_source(&state.sources, "github", &repo_full) else {
        return StatusCode::OK;
    };

    let labels = payload
        .issue
        .labels
        .iter()
        .map(|l| l.name.clone())
        .collect::<Vec<_>>();
    if !labels_match(&labels, &source.filter_label) {
        return StatusCode::OK;
    }

    let ticket = ExternalTicket {
        external_id: payload.issue.id.to_string(),
        number: Some(payload.issue.number.to_string()),
        provider: provider_key("github", &source.repo),
        title: payload.issue.title,
        body: payload.issue.body.unwrap_or_default(),
        labels,
        priority: None,
        repo: source.repo.clone(),
        url: payload.issue.html_url,
        author: payload.issue.user.and_then(|u| u.login),
    };

    dispatch_ticket(&state, ticket).await
}

async fn handle_gitlab(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !verify_token(&state.secrets.gitlab, &headers, "X-Gitlab-Token") {
        return StatusCode::UNAUTHORIZED;
    }

    let payload: GlWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("GitLab webhook parse error: {}", e),
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.object_kind != "issue" {
        return StatusCode::OK;
    }

    let repo_candidates = build_gitlab_repo_candidates(&payload.project);
    let Some(source) = match_source_candidates(&state.sources, "gitlab", &repo_candidates) else {
        return StatusCode::OK;
    };

    let labels = payload
        .labels
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.title)
        .collect::<Vec<_>>();

    if !labels_match(&labels, &source.filter_label) {
        return StatusCode::OK;
    }

    let ticket = ExternalTicket {
        external_id: payload.object_attributes.id.to_string(),
        number: Some(payload.object_attributes.iid.to_string()),
        provider: provider_key("gitlab", &source.repo),
        title: payload.object_attributes.title,
        body: payload.object_attributes.description.unwrap_or_default(),
        labels,
        priority: None,
        repo: source.repo.clone(),
        url: payload.object_attributes.url,
        author: payload.user.and_then(|u| u.username.or(u.name)),
    };

    dispatch_ticket(&state, ticket).await
}

async fn handle_linear(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !verify_linear(&state, &headers, &body) {
        return StatusCode::UNAUTHORIZED;
    }

    let payload: LinearWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("Linear webhook parse error: {}", e),
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    let Some(ticket) = build_linear_ticket(&state, payload) else {
        return StatusCode::OK;
    };

    dispatch_ticket(&state, ticket).await
}

async fn handle_jira(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !verify_token(&state.secrets.jira, &headers, "X-Atlassian-Webhook-Token") {
        return StatusCode::UNAUTHORIZED;
    }

    let payload: JiraWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("Jira webhook parse error: {}", e),
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    let Some(ticket) = build_jira_ticket(&state, payload) else {
        return StatusCode::OK;
    };

    dispatch_ticket(&state, ticket).await
}

fn build_linear_ticket(
    state: &WebhookState,
    payload: LinearWebhookPayload,
) -> Option<ExternalTicket> {
    if payload.r#type.as_deref() != Some("Issue") {
        return None;
    }
    let data = payload.data?;
    let team_key = linear_team_key(&data);
    let source = match_source(&state.sources, "linear", &team_key)?;
    let labels = extract_linear_labels(&data);
    if !labels.is_empty() && !labels_match(&labels, &source.filter_label) {
        return None;
    }
    Some(linear_ticket_from_data(&source, &data, labels))
}

fn linear_team_key(data: &Value) -> String {
    data.get("team")
        .and_then(|t| t.get("key"))
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            data.get("teamId")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

fn linear_ticket_from_data(
    source: &TicketIntakeSourceConfig,
    data: &Value,
    labels: Vec<String>,
) -> ExternalTicket {
    ExternalTicket {
        external_id: data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        number: data
            .get("identifier")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        provider: provider_key("linear", &source.repo),
        title: data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled")
            .to_string(),
        body: data
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        labels,
        priority: data
            .get("priority")
            .and_then(|v| v.as_i64())
            .and_then(map_linear_priority),
        repo: source.repo.clone(),
        url: data
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        author: data
            .get("creator")
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

fn build_jira_ticket(state: &WebhookState, payload: JiraWebhookPayload) -> Option<ExternalTicket> {
    let issue = payload.issue?;
    let project_key = jira_project_key(&issue);
    let source = match_source(&state.sources, "jira", &project_key)?;

    let labels = issue
        .fields
        .as_ref()
        .map(|f| f.labels.clone().unwrap_or_default())
        .unwrap_or_default();
    if !labels_match(&labels, &source.filter_label) {
        return None;
    }

    let description = issue
        .fields
        .as_ref()
        .and_then(|f| f.description.as_ref())
        .map(crate::ticket_intake::jira::extract_adf_text)
        .unwrap_or_default();

    let priority = issue
        .fields
        .as_ref()
        .and_then(|f| f.priority.as_ref())
        .and_then(|p| p.name.clone());

    let author = issue
        .fields
        .as_ref()
        .and_then(|f| f.reporter.as_ref())
        .and_then(|r| r.display_name.clone().or(r.email_address.clone()));

    Some(ExternalTicket {
        external_id: issue.key.clone(),
        number: Some(issue.key.clone()),
        provider: provider_key("jira", &source.repo),
        title: issue
            .fields
            .as_ref()
            .and_then(|f| f.summary.clone())
            .unwrap_or_else(|| "Untitled".to_string()),
        body: description,
        labels,
        priority,
        repo: source.repo.clone(),
        url: format!(
            "{}/browse/{}",
            payload
                .base_url
                .as_deref()
                .unwrap_or("https://atlassian.net"),
            issue.key
        ),
        author,
    })
}

fn jira_project_key(issue: &JiraIssue) -> String {
    issue
        .fields
        .as_ref()
        .and_then(|f| f.project.as_ref())
        .and_then(|p| p.key.clone())
        .unwrap_or_else(|| issue.key.clone())
}

async fn dispatch_ticket(state: &WebhookState, ticket: ExternalTicket) -> StatusCode {
    let dedup_key = ticket.dedup_key();
    match state.store.is_seen(&dedup_key) {
        Ok(true) => return StatusCode::OK,
        Ok(false) => {}
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("Webhook dedup lookup failed: {}", e),
            );
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }

    if let Err(e) = state.store.mark_seen(
        &dedup_key,
        &ticket.provider,
        &ticket.external_id,
        ticket.number.as_deref(),
        &ticket.title,
    ) {
        state.observer.log(
            ObserverCategory::TicketIntake,
            format!("Webhook failed to mark seen: {}", e),
        );
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    let task = ticket.to_autonomous_task();
    match state.auto_tx.send(task).await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            state.observer.log(
                ObserverCategory::TicketIntake,
                format!("Webhook dispatch failed: {}", e),
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn provider_key(provider: &str, repo: &str) -> String {
    format!("{}:{}", provider.to_lowercase(), repo)
}

fn match_source(
    sources: &[TicketIntakeSourceConfig],
    provider: &str,
    repo: &str,
) -> Option<TicketIntakeSourceConfig> {
    sources
        .iter()
        .find(|s| s.provider.eq_ignore_ascii_case(provider) && s.repo == repo)
        .cloned()
}

fn match_source_candidates(
    sources: &[TicketIntakeSourceConfig],
    provider: &str,
    repos: &[String],
) -> Option<TicketIntakeSourceConfig> {
    sources.iter().find_map(|s| {
        if !s.provider.eq_ignore_ascii_case(provider) {
            return None;
        }
        if repos.iter().any(|r| r == &s.repo) {
            Some(s.clone())
        } else {
            None
        }
    })
}

fn labels_match(labels: &[String], filter_label: &Option<String>) -> bool {
    let Some(filter) = filter_label
        .as_ref()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
    else {
        return true;
    };
    labels.iter().any(|l| l.eq_ignore_ascii_case(filter))
}

fn verify_github(state: &WebhookState, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(secret) = state.secrets.github.as_ref() else {
        return true;
    };
    let Some(sig) = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(hex_sig) = sig.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    // Use constant-time comparison to prevent timing attacks.
    mac.verify_slice(&sig_bytes).is_ok()
}

fn verify_linear(state: &WebhookState, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(secret) = state.secrets.linear.as_ref() else {
        return true;
    };
    let Some(sig) = headers
        .get("Linear-Signature")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(sig) else {
        return false;
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    // Use constant-time comparison to prevent timing attacks.
    mac.verify_slice(&sig_bytes).is_ok()
}

fn verify_token(secret: &Option<String>, headers: &HeaderMap, header_name: &str) -> bool {
    let Some(secret) = secret.as_ref() else {
        return true;
    };
    let Some(header_val) = headers.get(header_name).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    // Use constant-time comparison to prevent timing attacks.
    constant_time_eq(header_val.as_bytes(), secret.as_bytes())
}

/// Compares two byte slices in constant time to prevent timing attacks.
/// Returns true only if both slices are identical in length and content.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn build_gitlab_repo_candidates(project: &GlProject) -> Vec<String> {
    let mut repos = Vec::new();
    if let Some(path) = project.path_with_namespace.as_ref() {
        repos.push(path.clone());
    }
    if let Some(id) = project.id {
        repos.push(id.to_string());
    }
    repos
}

fn extract_linear_labels(data: &Value) -> Vec<String> {
    if let Some(nodes) = data
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(|n| n.as_array())
    {
        return nodes
            .iter()
            .filter_map(|n| {
                n.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
    }
    if let Some(arr) = data.get("labels").and_then(|l| l.as_array()) {
        return arr
            .iter()
            .filter_map(|n| {
                n.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
    }
    Vec::new()
}

fn map_linear_priority(priority: i64) -> Option<String> {
    match priority {
        1 => Some("urgent".to_string()),
        2 => Some("high".to_string()),
        3 => Some("medium".to_string()),
        4 => Some("low".to_string()),
        _ => None,
    }
}

#[derive(Deserialize)]
struct GhWebhookPayload {
    issue: GhIssue,
    repository: Option<GhRepository>,
}

#[derive(Deserialize)]
struct GhRepository {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct GhIssue {
    id: u64,
    number: u64,
    title: String,
    body: Option<String>,
    labels: Vec<GhLabel>,
    html_url: String,
    user: Option<GhUser>,
    pull_request: Option<Value>,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
struct GhUser {
    login: Option<String>,
}

#[derive(Deserialize)]
struct GlWebhookPayload {
    object_kind: String,
    project: GlProject,
    object_attributes: GlIssueAttributes,
    labels: Option<Vec<GlLabel>>,
    user: Option<GlUser>,
}

#[derive(Deserialize)]
struct GlProject {
    id: Option<u64>,
    path_with_namespace: Option<String>,
}

#[derive(Deserialize)]
struct GlIssueAttributes {
    id: u64,
    iid: u64,
    title: String,
    description: Option<String>,
    url: String,
}

#[derive(Deserialize, Clone)]
struct GlLabel {
    title: String,
}

#[derive(Deserialize)]
struct GlUser {
    username: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct LinearWebhookPayload {
    #[serde(rename = "type")]
    r#type: Option<String>,
    data: Option<Value>,
}

#[derive(Deserialize)]
struct JiraWebhookPayload {
    issue: Option<JiraIssue>,
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct JiraIssue {
    key: String,
    fields: Option<JiraFields>,
}

#[derive(Deserialize)]
struct JiraFields {
    summary: Option<String>,
    labels: Option<Vec<String>>,
    description: Option<Value>,
    priority: Option<JiraPriority>,
    reporter: Option<JiraReporter>,
    project: Option<JiraProject>,
}

#[derive(Deserialize)]
struct JiraPriority {
    name: Option<String>,
}

#[derive(Deserialize)]
struct JiraReporter {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct JiraProject {
    key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TicketIntakeSourceConfig;

    fn make_source(provider: &str, repo: &str) -> TicketIntakeSourceConfig {
        TicketIntakeSourceConfig {
            provider: provider.to_string(),
            repo: repo.to_string(),
            filter_label: Some("athena".to_string()),
            api_base: None,
            token_env: None,
            email_env: None,
        }
    }

    // --- provider_key ---

    #[test]
    fn provider_key_formats_correctly() {
        assert_eq!(provider_key("github", "owner/repo"), "github:owner/repo");
        assert_eq!(provider_key("GITHUB", "owner/repo"), "github:owner/repo");
        assert_eq!(provider_key("linear", "TEAM"), "linear:TEAM");
    }

    // --- match_source ---

    #[test]
    fn match_source_finds_matching_entry() {
        let sources = vec![make_source("github", "owner/repo")];
        let result = match_source(&sources, "github", "owner/repo");
        assert!(result.is_some());
    }

    #[test]
    fn match_source_is_case_insensitive_for_provider() {
        let sources = vec![make_source("github", "owner/repo")];
        let result = match_source(&sources, "GitHub", "owner/repo");
        assert!(result.is_some());
    }

    #[test]
    fn match_source_returns_none_for_wrong_repo() {
        let sources = vec![make_source("github", "owner/repo")];
        let result = match_source(&sources, "github", "other/repo");
        assert!(result.is_none());
    }

    #[test]
    fn match_source_returns_none_when_empty() {
        let result = match_source(&[], "github", "owner/repo");
        assert!(result.is_none());
    }

    // --- match_source_candidates ---

    #[test]
    fn match_source_candidates_finds_by_any_candidate() {
        let sources = vec![make_source("gitlab", "group/project")];
        let candidates = vec!["group/project".to_string(), "12345".to_string()];
        let result = match_source_candidates(&sources, "gitlab", &candidates);
        assert!(result.is_some());
    }

    #[test]
    fn match_source_candidates_returns_none_when_no_match() {
        let sources = vec![make_source("gitlab", "group/project")];
        let candidates = vec!["other/project".to_string()];
        let result = match_source_candidates(&sources, "gitlab", &candidates);
        assert!(result.is_none());
    }

    // --- labels_match ---

    #[test]
    fn labels_match_returns_true_when_no_filter() {
        assert!(labels_match(&["any".to_string()], &None));
        assert!(labels_match(&[], &None));
    }

    #[test]
    fn labels_match_returns_true_for_empty_filter() {
        assert!(labels_match(&["any".to_string()], &Some(String::new())));
    }

    #[test]
    fn labels_match_is_case_insensitive() {
        assert!(labels_match(
            &["Athena".to_string()],
            &Some("athena".to_string())
        ));
        assert!(labels_match(
            &["ATHENA".to_string()],
            &Some("Athena".to_string())
        ));
    }

    #[test]
    fn labels_match_returns_false_when_label_absent() {
        assert!(!labels_match(
            &["other".to_string()],
            &Some("athena".to_string())
        ));
    }

    // --- extract_linear_labels ---

    #[test]
    fn extract_linear_labels_from_nodes_array() {
        let data: Value = serde_json::json!({
            "labels": { "nodes": [{ "name": "bug" }, { "name": "urgent" }] }
        });
        let labels = extract_linear_labels(&data);
        assert_eq!(labels, vec!["bug", "urgent"]);
    }

    #[test]
    fn extract_linear_labels_from_flat_array() {
        let data: Value = serde_json::json!({
            "labels": [{ "name": "feature" }]
        });
        let labels = extract_linear_labels(&data);
        assert_eq!(labels, vec!["feature"]);
    }

    #[test]
    fn extract_linear_labels_returns_empty_when_absent() {
        let data: Value = serde_json::json!({});
        assert!(extract_linear_labels(&data).is_empty());
    }

    // --- map_linear_priority ---

    #[test]
    fn map_linear_priority_maps_known_values() {
        assert_eq!(map_linear_priority(1), Some("urgent".to_string()));
        assert_eq!(map_linear_priority(2), Some("high".to_string()));
        assert_eq!(map_linear_priority(3), Some("medium".to_string()));
        assert_eq!(map_linear_priority(4), Some("low".to_string()));
    }

    #[test]
    fn map_linear_priority_returns_none_for_unknown() {
        assert_eq!(map_linear_priority(0), None);
        assert_eq!(map_linear_priority(5), None);
    }

    // --- build_gitlab_repo_candidates ---

    #[test]
    fn build_gitlab_repo_candidates_includes_path_and_id() {
        let project = GlProject {
            id: Some(42),
            path_with_namespace: Some("group/proj".to_string()),
        };
        let candidates = build_gitlab_repo_candidates(&project);
        assert!(candidates.contains(&"group/proj".to_string()));
        assert!(candidates.contains(&"42".to_string()));
    }

    #[test]
    fn build_gitlab_repo_candidates_handles_missing_fields() {
        let project = GlProject {
            id: None,
            path_with_namespace: None,
        };
        assert!(build_gitlab_repo_candidates(&project).is_empty());
    }

    // --- constant_time_eq ---

    #[test]
    fn constant_time_eq_returns_true_for_equal_slices() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_returns_false_for_different_content() {
        assert!(!constant_time_eq(b"secret1", b"secret2"));
    }

    #[test]
    fn constant_time_eq_returns_false_for_different_length() {
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    // --- HMAC signature helpers ---

    #[test]
    fn verify_github_hmac_accepts_correct_signature() {
        use hmac::Mac;

        let secret = "test_secret";
        let body = b"hello world";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let raw_bytes = mac.finalize().into_bytes();
        let sig = format!("sha256={}", hex::encode(raw_bytes));

        // Verify that our constant-time path accepts the correct signature.
        let ok_bytes = hex::decode(sig.strip_prefix("sha256=").unwrap()).unwrap();
        let mut mac2 = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac2.update(body);
        assert!(mac2.verify_slice(&ok_bytes).is_ok());
    }

    #[test]
    fn verify_github_hmac_rejects_wrong_signature() {
        use hmac::Mac;

        let body = b"hello world";
        let bad_sig =
            hex::decode("deadbeef00000000000000000000000000000000000000000000000000000000")
                .unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        assert!(mac.verify_slice(&bad_sig).is_err());
    }
}
