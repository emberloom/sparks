use async_trait::async_trait;

use crate::core::AutonomousTask;
use crate::error::Result;
use crate::kpi;
use crate::pulse::PulseTarget;

#[derive(Debug, Clone)]
pub struct ExternalTicket {
    pub external_id: String,
    pub provider: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub priority: Option<String>,
    pub repo: String,
    pub url: String,
    pub author: Option<String>,
}

impl ExternalTicket {
    pub fn dedup_key(&self) -> String {
        format!("{}:{}", self.provider, self.external_id)
    }

    pub fn risk_tier(&self) -> String {
        let label_hint = self
            .labels
            .iter()
            .map(|l| l.to_lowercase())
            .collect::<Vec<_>>();

        if label_hint.iter().any(|l| l.contains("critical") || l.contains("p0")) {
            return "critical".to_string();
        }
        if label_hint.iter().any(|l| l.contains("high") || l.contains("p1")) {
            return "high".to_string();
        }
        if label_hint.iter().any(|l| l.contains("low") || l.contains("p3")) {
            return "low".to_string();
        }

        if let Some(priority) = self.priority.as_ref() {
            let p = priority.to_lowercase();
            if p.contains("urgent") || p.contains("critical") || p.contains("p0") {
                return "critical".to_string();
            }
            if p.contains("high") || p.contains("p1") {
                return "high".to_string();
            }
            if p.contains("low") || p.contains("p3") {
                return "low".to_string();
            }
        }

        "medium".to_string()
    }

    pub fn ghost_hint(&self) -> Option<String> {
        for label in &self.labels {
            let lower = label.to_lowercase();
            if let Some(rest) = lower.strip_prefix("athena:") {
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    pub fn to_autonomous_task(&self) -> AutonomousTask {
        let goal = format!("Address ticket: {}", self.title.trim());
        let labels = if self.labels.is_empty() {
            "none".to_string()
        } else {
            self.labels.join(", ")
        };
        let priority = self.priority.clone().unwrap_or_else(|| "none".to_string());
        let author = self
            .author
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or("unknown");

        let mut context = format!(
            "Source: {}\nRepo: {}\nURL: {}\nAuthor: {}\nLabels: {}\nPriority: {}",
            self.provider, self.repo, self.url, author, labels, priority
        );

        if !self.body.trim().is_empty() {
            context.push_str("\n\nDescription:\n");
            context.push_str(self.body.trim());
        }

        let repo = if self.repo.trim().is_empty() {
            kpi::default_repo_name()
        } else {
            self.repo.clone()
        };

        AutonomousTask {
            goal,
            context,
            ghost: self.ghost_hint(),
            target: PulseTarget::Broadcast,
            lane: "ticket_intake".to_string(),
            risk_tier: self.risk_tier(),
            repo,
            task_id: Some(format!("ticket:{}", self.dedup_key())),
        }
    }
}

#[async_trait]
pub trait TicketProvider: Send + Sync {
    fn name(&self) -> String;
    async fn poll(&self) -> Result<Vec<ExternalTicket>>;
}
