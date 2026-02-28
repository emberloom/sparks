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

fn load_webhook_secrets(config: &TicketIntakeWebhookConfig, observer: &ObserverHandle) -> WebhookSecrets {
    let mut secrets = WebhookSecrets::default();
    secrets.github = read_env_opt(config.github_secret_env.as_deref(), observer, "github");
    secrets.gitlab = read_env_opt(config.gitlab_secret_env.as_deref(), observer, "gitlab");
    secrets.linear = read_env_opt(config.linear_secret_env.as_deref(), observer, "linear");
    secrets.jira = read_env_opt(config.jira_secret_env.as_deref(), observer, "jira");
    secrets
}

fn read_env_opt(env_key: Option<&str>, observer: &ObserverHandle, label: &str) -> Option<String> {
    let Some(key) = env_key else { return None; };
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

    let labels = payload.issue.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>();
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

    if payload.r#type.as_deref() != Some("Issue") {
        return StatusCode::OK;
    }

    let data = match payload.data {
        Some(d) => d,
        None => return StatusCode::OK,
    };

    let team_key = data
        .get("team")
        .and_then(|t| t.get("key"))
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .or_else(|| data.get("teamId").and_then(|t| t.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();

    let Some(source) = match_source(&state.sources, "linear", &team_key) else {
        return StatusCode::OK;
    };

    let labels = extract_linear_labels(&data);
    if !labels.is_empty() && !labels_match(&labels, &source.filter_label) {
        return StatusCode::OK;
    }

    let ticket = ExternalTicket {
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

    let issue = match payload.issue {
        Some(i) => i,
        None => return StatusCode::OK,
    };

    let project_key = issue
        .fields
        .as_ref()
        .and_then(|f| f.project.as_ref())
        .and_then(|p| p.key.clone())
        .unwrap_or_else(|| issue.key.clone());

    let Some(source) = match_source(&state.sources, "jira", &project_key) else {
        return StatusCode::OK;
    };

    let labels = issue
        .fields
        .as_ref()
        .map(|f| f.labels.clone().unwrap_or_default())
        .unwrap_or_default();
    if !labels_match(&labels, &source.filter_label) {
        return StatusCode::OK;
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

    let ticket = ExternalTicket {
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
    };

    dispatch_ticket(&state, ticket).await
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

fn match_source(sources: &[TicketIntakeSourceConfig], provider: &str, repo: &str) -> Option<TicketIntakeSourceConfig> {
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
    let Some(filter) = filter_label.as_ref().map(|l| l.trim()).filter(|l| !l.is_empty()) else {
        return true;
    };
    labels
        .iter()
        .any(|l| l.eq_ignore_ascii_case(filter))
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

    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    sig == expected
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

    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    sig == expected
}

fn verify_token(secret: &Option<String>, headers: &HeaderMap, header_name: &str) -> bool {
    let Some(secret) = secret.as_ref() else {
        return true;
    };
    headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == secret)
        .unwrap_or(false)
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
    if let Some(nodes) = data.get("labels").and_then(|l| l.get("nodes")).and_then(|n| n.as_array()) {
        return nodes
            .iter()
            .filter_map(|n| n.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
    }
    if let Some(arr) = data.get("labels").and_then(|l| l.as_array()) {
        return arr
            .iter()
            .filter_map(|n| n.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
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
