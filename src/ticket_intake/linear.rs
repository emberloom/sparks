use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AthenaError, Result};
use crate::ticket_intake::provider::{ExternalTicket, TicketProvider};

#[derive(Clone)]
pub struct LinearProvider {
    client: reqwest::Client,
    team_key: String,
    filter_label: String,
    token: String,
}

impl LinearProvider {
    pub fn new(
        client: reqwest::Client,
        team_key: String,
        filter_label: String,
        token: String,
    ) -> Self {
        Self {
            client,
            team_key,
            filter_label,
            token,
        }
    }

    async fn post_graphql(&self, payload: serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post("https://api.linear.app/graphql")
            .header("Authorization", self.token.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("Linear request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("Linear response error: {}", e)))?;

        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("Linear parse failed: {}", e)))?;

        if let Some(errors) = value.get("errors").and_then(|v| v.as_array()) {
            let messages = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>();
            if !messages.is_empty() {
                return Err(AthenaError::Tool(format!(
                    "Linear API errors: {}",
                    messages.join("; ")
                )));
            }
        }

        Ok(value)
    }

    async fn fetch_completed_state_id(&self, issue_id: &str) -> Result<Option<String>> {
        let payload = json!({
            "query": LINEAR_ISSUE_STATE_QUERY,
            "variables": { "id": issue_id }
        });
        let value = self.post_graphql(payload).await?;
        let Some(nodes) = value
            .get("data")
            .and_then(|d| d.get("issue"))
            .and_then(|i| i.get("team"))
            .and_then(|t| t.get("states"))
            .and_then(|s| s.get("nodes"))
            .and_then(|n| n.as_array())
        else {
            return Ok(None);
        };

        for node in nodes {
            let state_type = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if state_type.eq_ignore_ascii_case("completed") {
                if let Some(id) = node.get("id").and_then(|i| i.as_str()) {
                    return Ok(Some(id.to_string()));
                }
            }
        }

        Ok(None)
    }
}

const LINEAR_QUERY: &str = r#"
query Issues($team: String!, $label: String!) {
  issues(
    first: 20,
    orderBy: createdAt,
    filter: {
      team: { key: { eq: $team } }
      labels: { name: { eq: $label } }
      state: { type: { nin: ["completed", "cancelled"] } }
    }
  ) {
    nodes {
      id
      identifier
      title
      description
      url
      priority
      labels {
        nodes { name }
      }
      creator { name }
    }
  }
}
"#;

const LINEAR_COMMENT_MUTATION: &str = r#"
mutation CommentCreate($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) {
    success
  }
}
"#;

const LINEAR_ISSUE_STATE_QUERY: &str = r#"
query IssueState($id: String!) {
  issue(id: $id) {
    id
    team {
      states {
        nodes { id type name }
      }
    }
  }
}
"#;

const LINEAR_ISSUE_UPDATE_MUTATION: &str = r#"
mutation IssueUpdate($id: String!, $stateId: String!) {
  issueUpdate(id: $id, input: { stateId: $stateId }) {
    success
  }
}
"#;

#[derive(Serialize)]
struct LinearRequest<'a> {
    query: &'a str,
    variables: LinearVariables<'a>,
}

#[derive(Serialize)]
struct LinearVariables<'a> {
    team: &'a str,
    label: &'a str,
}

#[derive(Deserialize)]
struct LinearResponse {
    data: Option<LinearData>,
    errors: Option<Vec<LinearError>>,
}

#[derive(Deserialize)]
struct LinearError {
    message: String,
}

#[derive(Deserialize)]
struct LinearData {
    issues: LinearIssues,
}

#[derive(Deserialize)]
struct LinearIssues {
    nodes: Vec<LinearIssue>,
}

#[derive(Deserialize)]
struct LinearIssue {
    id: String,
    identifier: Option<String>,
    title: String,
    description: Option<String>,
    url: String,
    priority: Option<i64>,
    labels: LinearLabels,
    creator: Option<LinearUser>,
}

#[derive(Deserialize)]
struct LinearLabels {
    nodes: Vec<LinearLabel>,
}

#[derive(Deserialize)]
struct LinearLabel {
    name: String,
}

#[derive(Deserialize)]
struct LinearUser {
    name: Option<String>,
}

#[async_trait]
impl TicketProvider for LinearProvider {
    fn name(&self) -> String {
        format!("linear:{}", self.team_key)
    }

    async fn poll(&self) -> Result<Vec<ExternalTicket>> {
        let provider_name = self.name();
        let body = LinearRequest {
            query: LINEAR_QUERY,
            variables: LinearVariables {
                team: &self.team_key,
                label: &self.filter_label,
            },
        };

        let resp = self
            .client
            .post("https://api.linear.app/graphql")
            .header("Authorization", self.token.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| AthenaError::Tool(format!("Linear request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| AthenaError::Tool(format!("Linear response error: {}", e)))?;

        let payload: LinearResponse = resp
            .json()
            .await
            .map_err(|e| AthenaError::Tool(format!("Linear parse failed: {}", e)))?;

        if let Some(errors) = payload.errors {
            let messages = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>();
            return Err(AthenaError::Tool(format!(
                "Linear API errors: {}",
                messages.join("; ")
            )));
        }

        let issues = payload
            .data
            .map(|d| d.issues.nodes)
            .unwrap_or_default();

        let tickets = issues
            .into_iter()
            .map(|issue| ExternalTicket {
                external_id: issue.id,
                number: issue.identifier.clone(),
                provider: provider_name.clone(),
                title: issue.title,
                body: issue.description.unwrap_or_default(),
                labels: issue.labels.nodes.into_iter().map(|l| l.name).collect(),
                priority: map_linear_priority(issue.priority),
                repo: self.team_key.clone(),
                url: issue.url,
                author: issue.creator.and_then(|u| u.name),
            })
            .collect::<Vec<_>>();

        Ok(tickets)
    }

    async fn post_comment(&self, ticket: &ExternalTicket, message: &str) -> Result<()> {
        let payload = json!({
            "query": LINEAR_COMMENT_MUTATION,
            "variables": { "issueId": ticket.external_id, "body": message }
        });
        self.post_graphql(payload)
            .await
            .map(|_| ())
    }

    async fn update_status(&self, ticket: &ExternalTicket, status: &str) -> Result<()> {
        if status != "succeeded" {
            return Ok(());
        }
        let Some(state_id) = self.fetch_completed_state_id(&ticket.external_id).await? else {
            return Ok(());
        };
        let payload = json!({
            "query": LINEAR_ISSUE_UPDATE_MUTATION,
            "variables": { "id": ticket.external_id, "stateId": state_id }
        });
        self.post_graphql(payload).await.map(|_| ())
    }

    fn supports_writeback(&self) -> bool {
        true
    }
}

fn map_linear_priority(priority: Option<i64>) -> Option<String> {
    match priority {
        Some(1) => Some("urgent".to_string()),
        Some(2) => Some("high".to_string()),
        Some(3) => Some("medium".to_string()),
        Some(4) => Some("low".to_string()),
        _ => None,
    }
}
