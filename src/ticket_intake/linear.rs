use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
                provider: "linear".to_string(),
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
