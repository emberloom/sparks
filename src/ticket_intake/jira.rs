use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

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
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

#[async_trait]
impl TicketProvider for JiraProvider {
    fn name(&self) -> String {
        format!("jira:{}", self.project_key)
    }

    async fn poll(&self) -> Result<Vec<ExternalTicket>> {
        let provider_name = self.name();
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
                    .and_then(|u| u.display_name.or(u.email_address));

                ExternalTicket {
                    external_id: issue.key.clone(),
                    number: Some(issue.key.clone()),
                    provider: provider_name.clone(),
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

    async fn post_comment(&self, ticket: &ExternalTicket, message: &str) -> Result<()> {
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "Jira write-back missing issue key".to_string(),
            ));
        };
        let url = format!(
            "{}/rest/api/3/issue/{}/comment",
            self.base_url.trim_end_matches('/'),
            number
        );
        let auth = STANDARD.encode(format!("{}:{}", self.email, self.api_token));
        let body = json!({
            "body": {
                "type": "doc",
                "version": 1,
                "content": [{
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": message }]
                }]
            }
        });
        self.client
            .post(&url)
            .header("Authorization", format!("Basic {}", auth))
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("Jira comment failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("Jira comment error: {}", e)))?;
        Ok(())
    }

    async fn update_status(&self, ticket: &ExternalTicket, status: &str) -> Result<()> {
        if status != "succeeded" {
            return Ok(());
        }
        let Some(number) = ticket.number.as_ref() else {
            return Err(AthenaError::Tool(
                "Jira write-back missing issue key".to_string(),
            ));
        };
        let auth = STANDARD.encode(format!("{}:{}", self.email, self.api_token));
        let base = self.base_url.trim_end_matches('/');
        let transitions_url = format!("{}/rest/api/3/issue/{}/transitions", base, number);
        let resp = self
            .client
            .get(&transitions_url)
            .header("Authorization", format!("Basic {}", auth))
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("Jira transitions failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("Jira transitions error: {}", e)))?;

        let payload: Value = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("Jira transitions parse failed: {}", e)))?;
        let transitions = payload
            .get("transitions")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        let mut chosen: Option<String> = None;
        for t in transitions {
            let name = t
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            let target = t
                .get("to")
                .and_then(|to| to.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            if ["done", "closed", "resolved"].iter().any(|k| name.contains(k) || target.contains(k)) {
                chosen = t.get("id").and_then(|id| id.as_str()).map(|s| s.to_string());
                if chosen.is_some() {
                    break;
                }
            }
        }

        let Some(transition_id) = chosen else {
            return Ok(());
        };

        let update_body = json!({ "transition": { "id": transition_id } });
        self.client
            .post(&transitions_url)
            .header("Authorization", format!("Basic {}", auth))
            .json(&update_body)
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("Jira status update failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("Jira status update error: {}", e)))?;
        Ok(())
    }

    fn supports_writeback(&self) -> bool {
        true
    }
}

pub(crate) fn extract_adf_text(value: &Value) -> String {
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
