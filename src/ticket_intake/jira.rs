use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{AthenaError, Result};
use crate::ticket_intake::provider::{ExternalTicket, TicketProvider};

#[derive(Clone)]
pub struct JiraProvider {
    client: reqwest::Client,
    project_key: String,
    filter_label: String,
    base_url: String,
    email: String,
    api_token: String,
}

impl JiraProvider {
    pub fn new(
        client: reqwest::Client,
        project_key: String,
        filter_label: String,
        base_url: String,
        email: String,
        api_token: String,
    ) -> Self {
        Self {
            client,
            project_key,
            filter_label,
            base_url,
            email,
            api_token,
        }
    }
}

#[derive(Deserialize)]
struct JiraSearchResponse {
    issues: Vec<JiraIssue>,
}

#[derive(Deserialize)]
struct JiraIssue {
    id: String,
    key: String,
    fields: JiraFields,
}

#[derive(Deserialize)]
struct JiraFields {
    summary: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    priority: Option<JiraPriority>,
    #[serde(default)]
    reporter: Option<JiraUser>,
}

#[derive(Deserialize)]
struct JiraPriority {
    name: String,
}

#[derive(Deserialize)]
struct JiraUser {
    displayName: Option<String>,
    emailAddress: Option<String>,
}

#[async_trait]
impl TicketProvider for JiraProvider {
    fn name(&self) -> String {
        format!("jira:{}", self.project_key)
    }

    async fn poll(&self) -> Result<Vec<ExternalTicket>> {
        let label = if self.filter_label.contains(' ') {
            format!("\"{}\"", self.filter_label)
        } else {
            self.filter_label.clone()
        };
        let jql = format!(
            "project = {} AND labels = {} AND status != Done ORDER BY created DESC",
            self.project_key, label
        );

        let url = format!(
            "{}/rest/api/3/search",
            self.base_url.trim_end_matches('/')
        );
        let auth = STANDARD.encode(format!("{}:{}", self.email, self.api_token));

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Basic {}", auth))
            .query(&[("jql", jql), ("maxResults", "20".to_string())])
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("JIRA request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("JIRA response error: {}", e)))?;

        let payload: JiraSearchResponse = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("JIRA parse failed: {}", e)))?;

        let tickets = payload
            .issues
            .into_iter()
            .map(|issue| {
                let description = issue
                    .fields
                    .description
                    .as_ref()
                    .map(extract_adf_text)
                    .unwrap_or_default();

                let author = issue
                    .fields
                    .reporter
                    .and_then(|u| u.displayName.or(u.emailAddress));

                ExternalTicket {
                    external_id: issue.key.clone(),
                    provider: "jira".to_string(),
                    title: issue.fields.summary,
                    body: description,
                    labels: issue.fields.labels,
                    priority: issue.fields.priority.map(|p| p.name),
                    repo: self.project_key.clone(),
                    url: format!(
                        "{}/browse/{}",
                        self.base_url.trim_end_matches('/'),
                        issue.key
                    ),
                    author,
                }
            })
            .collect::<Vec<_>>();

        Ok(tickets)
    }
}

fn extract_adf_text(value: &Value) -> String {
    let mut parts = Vec::new();
    collect_adf_text(value, &mut parts);
    parts
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn collect_adf_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get("text") {
                parts.push(text.clone());
            }
            if let Some(Value::Array(content)) = map.get("content") {
                for child in content {
                    collect_adf_text(child, parts);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_adf_text(item, parts);
            }
        }
        _ => {}
    }
}
