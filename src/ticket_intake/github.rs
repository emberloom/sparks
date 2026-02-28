use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;
use serde_json::json;

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
        let provider_name = self.name();
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
                number: Some(issue.number.to_string()),
                provider: provider_name.clone(),
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

    async fn post_comment(&self, ticket: &ExternalTicket, message: &str) -> Result<()> {
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "GitHub write-back missing issue number".to_string(),
            ));
        };

        let url = format!(
            "{}/repos/{}/issues/{}/comments",
            self.api_base.trim_end_matches('/'),
            self.repo,
            number
        );
        self.client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT, "application/vnd.github+json")
            .json(&json!({ "body": message }))
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitHub comment failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitHub comment error: {}", e)))?;
        Ok(())
    }

    async fn update_status(&self, ticket: &ExternalTicket, status: &str) -> Result<()> {
        if status != "succeeded" {
            return Ok(());
        }
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "GitHub write-back missing issue number".to_string(),
            ));
        };
        let url = format!(
            "{}/repos/{}/issues/{}",
            self.api_base.trim_end_matches('/'),
            self.repo,
            number
        );
        self.client
            .patch(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT, "application/vnd.github+json")
            .json(&json!({ "state": "closed" }))
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitHub status update failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitHub status update error: {}", e)))?;
        Ok(())
    }

    fn supports_writeback(&self) -> bool {
        true
    }
}
