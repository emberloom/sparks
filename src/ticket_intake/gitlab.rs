use async_trait::async_trait;
use reqwest::header::HeaderName;
use serde::Deserialize;

use crate::error::{AthenaError, Result};
use crate::ticket_intake::provider::{ExternalTicket, TicketProvider};

#[derive(Clone)]
pub struct GitLabProvider {
    client: reqwest::Client,
    project_id: String,
    filter_label: String,
    token: String,
    api_base: String,
}

impl GitLabProvider {
    pub fn new(
        client: reqwest::Client,
        project_id: String,
        filter_label: String,
        token: String,
        api_base: String,
    ) -> Self {
        Self {
            client,
            project_id,
            filter_label,
            token,
            api_base,
        }
    }
}

#[derive(Deserialize)]
struct GlIssue {
    id: u64,
    title: String,
    description: Option<String>,
    labels: Vec<String>,
    web_url: String,
    author: Option<GlAuthor>,
}

#[derive(Deserialize)]
struct GlAuthor {
    username: Option<String>,
    name: Option<String>,
}

#[async_trait]
impl TicketProvider for GitLabProvider {
    fn name(&self) -> String {
        format!("gitlab:{}", self.project_id)
    }

    async fn poll(&self) -> Result<Vec<ExternalTicket>> {
        let encoded = encode_project_id(&self.project_id);
        let url = format!(
            "{}/projects/{}/issues",
            self.api_base.trim_end_matches('/'),
            encoded
        );

        let token_header = HeaderName::from_static("private-token");
        let resp = self
            .client
            .get(&url)
            .header(token_header, self.token.clone())
            .query(&[
                ("labels", self.filter_label.as_str()),
                ("state", "opened"),
                ("per_page", "20"),
                ("order_by", "created_at"),
                ("sort", "desc"),
            ])
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitLab request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitLab response error: {}", e)))?;

        let issues: Vec<GlIssue> = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitLab parse failed: {}", e)))?;

        let tickets = issues
            .into_iter()
            .map(|issue| ExternalTicket {
                external_id: issue.id.to_string(),
                provider: "gitlab".to_string(),
                title: issue.title,
                body: issue.description.unwrap_or_default(),
                labels: issue.labels,
                priority: None,
                repo: self.project_id.clone(),
                url: issue.web_url,
                author: issue.author.and_then(|a| a.username.or(a.name)),
            })
            .collect::<Vec<_>>();

        Ok(tickets)
    }
}

fn encode_project_id(id: &str) -> String {
    id.replace('/', "%2F")
}
