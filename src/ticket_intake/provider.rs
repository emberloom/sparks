use async_trait::async_trait;

use crate::core::AutonomousTask;
use crate::error::Result;
use crate::kpi;
use crate::pulse::PulseTarget;

#[derive(Debug, Clone)]
pub struct ExternalTicket {
    pub external_id: String,
    pub number: Option<String>,
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

        if label_hint
            .iter()
            .any(|l| l.contains("critical") || l.contains("p0"))
        {
            return "critical".to_string();
        }
        if label_hint
            .iter()
            .any(|l| l.contains("high") || l.contains("p1"))
        {
            return "high".to_string();
        }
        if label_hint
            .iter()
            .any(|l| l.contains("low") || l.contains("p3"))
        {
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
            if let Some(rest) = lower.strip_prefix("sparks:") {
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
        let number = self.number.as_ref().map(|n| n.as_str()).unwrap_or("n/a");
        let author = self
            .author
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or("unknown");

        let mut context = format!(
            "Source: {}\nRepo: {}\nNumber: {}\nURL: {}\nAuthor: {}\nLabels: {}\nPriority: {}",
            self.provider, self.repo, number, self.url, author, labels, priority
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

/// Rich supplementary context fetched on demand (comments, diffs).
#[derive(Debug, Default)]
pub struct TicketContext {
    pub comments: Vec<String>,
    /// For PRs: unified diff summary (first N lines).
    pub diff_summary: Option<String>,
}

impl TicketContext {
    /// Format into a markdown block, trimming to `char_cap` characters.
    ///
    /// Trim order when over budget: comments first (drop oldest), then diff.
    pub fn format(&self, char_cap: usize) -> String {
        // Attempt to fit everything; if over budget, try dropping comments.
        let full = self.format_inner(&self.comments, self.diff_summary.as_deref());
        if full.len() <= char_cap {
            return full;
        }

        // Drop comments progressively from the end until we fit.
        for keep in (0..self.comments.len()).rev() {
            let partial = self.format_inner(&self.comments[..keep], self.diff_summary.as_deref());
            if partial.len() <= char_cap {
                return partial;
            }
        }

        // Still too long — drop diff too, keep only what fits.
        let no_diff = self.format_inner(&[], None);
        if no_diff.len() <= char_cap {
            return no_diff;
        }

        // Hard truncate as last resort.
        let mut s = no_diff;
        s.truncate(char_cap);
        s
    }

    fn format_inner(&self, comments: &[String], diff: Option<&str>) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !comments.is_empty() {
            parts.push("### Comments\n".to_string());
            for c in comments {
                parts.push(format!("- {}\n", c));
            }
        }
        if let Some(d) = diff {
            parts.push(format!("### Diff\n```diff\n{}\n```\n", d));
        }
        parts.join("")
    }
}

#[async_trait]
pub trait TicketProvider: Send + Sync {
    fn name(&self) -> String;
    async fn poll(&self) -> Result<Vec<ExternalTicket>>;
    async fn post_comment(&self, _ticket: &ExternalTicket, _message: &str) -> Result<()> {
        Ok(())
    }
    async fn update_status(&self, _ticket: &ExternalTicket, _status: &str) -> Result<()> {
        Ok(())
    }
    fn supports_writeback(&self) -> bool {
        false
    }
    /// Fetch rich supplementary context for a ticket (comments, diff).
    /// Providers that don't implement this return an empty `TicketContext`.
    async fn fetch_full_context(&self, _ticket: &ExternalTicket) -> Result<TicketContext> {
        Ok(TicketContext::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_context_format_includes_all_sections() {
        let ctx = TicketContext {
            comments: vec!["comment one".into(), "comment two".into()],
            diff_summary: Some("+ added line\n- removed line".into()),
        };
        let formatted = ctx.format(4000);
        assert!(formatted.contains("comment one"));
        assert!(formatted.contains("+ added line"));
    }

    #[test]
    fn ticket_context_format_trims_to_char_cap() {
        let long_comment = "x".repeat(5000);
        let ctx = TicketContext {
            comments: vec![long_comment],
            diff_summary: None,
        };
        let formatted = ctx.format(1000);
        assert!(formatted.len() <= 1100); // small headroom for section headers
    }
}
