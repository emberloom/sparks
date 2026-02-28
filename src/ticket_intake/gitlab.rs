use async_trait::async_trait;
use reqwest::header::HeaderName;
use serde::Deserialize;
use serde_json::json;

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
    iid: u64,
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
        let provider_name = self.name();
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
                number: Some(issue.iid.to_string()),
                provider: provider_name.clone(),
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

    async fn post_comment(&self, ticket: &ExternalTicket, message: &str) -> Result<()> {
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "GitLab write-back missing issue number".to_string(),
            ));
        };
        let encoded = encode_project_id(&self.project_id);
        let url = format!(
            "{}/projects/{}/issues/{}/notes",
            self.api_base.trim_end_matches('/'),
            encoded,
            number
        );
        let token_header = HeaderName::from_static("private-token");
        self.client
            .post(&url)
            .header(token_header, self.token.clone())
            .json(&json!({ "body": message }))
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitLab comment failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitLab comment error: {}", e)))?;
        Ok(())
    }

    async fn update_status(&self, ticket: &ExternalTicket, status: &str) -> Result<()> {
        if status != "succeeded" {
            return Ok(());
        }
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "GitLab write-back missing issue number".to_string(),
            ));
        };
        let encoded = encode_project_id(&self.project_id);
        let url = format!(
            "{}/projects/{}/issues/{}",
            self.api_base.trim_end_matches('/'),
            encoded,
            number
        );
        let token_header = HeaderName::from_static("private-token");
        self.client
            .put(&url)
            .header(token_header, self.token.clone())
            .json(&json!({ "state_event": "close" }))
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("GitLab status update failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("GitLab status update error: {}", e)))?;
        Ok(())
    }

    fn supports_writeback(&self) -> bool {
        true
    }
}

fn encode_project_id(id: &str) -> String {
    id.replace('/', "%2F")
}
