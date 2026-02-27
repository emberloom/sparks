use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;

use crate::error::{AthenaError, Result};
use crate::ticket_intake::provider::{ExternalTicket, TicketProvider};

#[derive(Clone)]
pub struct GitHubProvider {
    client: reqwest::Client,
    repo: String,
    filter_label: String,
    token: String,
    api_base: String,
}

impl GitHubProvider {
    pub fn new(
        client: reqwest::Client,
        repo: String,
        filter_label: String,
        token: String,
        api_base: String,
    ) -> Self {
        Self {
            client,
            repo,
            filter_label,
            token,
            api_base,
        }
    }
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
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[async_trait]
impl TicketProvider for GitHubProvider {
    fn name(&self) -> String {
        format!("github:{}", self.repo)
    }

    async fn poll(&self) -> Result<Vec<ExternalTicket>> {
        let url = format!(
            "{}/repos/{}/issues",
            self.api_base.trim_end_matches('/'),
            self.repo
        );

        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT, "application/vnd.github+json")
            .query(&[
                ("labels", self.filter_label.as_str()),
                ("state", "open"),
                ("per_page", "20"),
                ("sort", "created"),
                ("direction", "desc"),
            ])
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitHub request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitHub response error: {}", e)))?;

        let issues: Vec<GhIssue> = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitHub parse failed: {}", e)))?;

        let tickets = issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .map(|issue| ExternalTicket {
                external_id: issue.id.to_string(),
                provider: "github".to_string(),
                title: issue.title,
                body: issue.body.unwrap_or_default(),
                labels: issue.labels.into_iter().map(|l| l.name).collect(),
                priority: None,
                repo: self.repo.clone(),
                url: issue.html_url,
                author: issue.user.map(|u| u.login),
            })
            .collect::<Vec<_>>();

        Ok(tickets)
    }
}
