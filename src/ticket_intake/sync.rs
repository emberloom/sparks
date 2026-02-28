use std::collections::HashMap;
use std::sync::Arc;

use tokio::time::{Duration, MissedTickBehavior};

use crate::knobs::SharedKnobs;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::ticket_intake::provider::ExternalTicket;
use crate::ticket_intake::store::TicketSyncRecord;
use crate::ticket_intake::{TicketIntakeStore, TicketProvider};

const SYNC_INTERVAL_SECS: u64 = 30;

pub fn spawn_ticket_status_sync(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    store: Arc<TicketIntakeStore>,
    providers: HashMap<String, Arc<dyn TicketProvider>>,
    webhook_enabled: bool,
) {
    if providers.is_empty() {
        return;
    }

    let writeback_count = providers.values().filter(|p| p.supports_writeback()).count();
    if writeback_count == 0 {
        observer.log(
            ObserverCategory::TicketIntake,
            "Ticket status sync skipped: no providers support write-back",
        );
        return;
    }

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(SYNC_INTERVAL_SECS));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tick.tick().await;

            if !should_run_sync(&knobs, webhook_enabled) {
                continue;
            }

            process_pending_syncs(&observer, store.as_ref(), &providers).await;
        }
    });
}

fn should_run_sync(knobs: &SharedKnobs, webhook_enabled: bool) -> bool {
    let poll_enabled = {
        let k = knobs.read().unwrap();
        k.ticket_intake_enabled
    };
    poll_enabled || webhook_enabled
}

async fn process_pending_syncs(
    observer: &ObserverHandle,
    store: &TicketIntakeStore,
    providers: &HashMap<String, Arc<dyn TicketProvider>>,
) {
    let pending = match store.get_pending_syncs(25) {
        Ok(rows) => rows,
        Err(e) => {
            observer.log(
                ObserverCategory::TicketIntake,
                format!("Ticket sync query failed: {}", e),
            );
            return;
        }
    };

    if pending.is_empty() {
        return;
    }

    for item in pending {
        sync_record(observer, store, providers, item).await;
    }
}

async fn sync_record(
    observer: &ObserverHandle,
    store: &TicketIntakeStore,
    providers: &HashMap<String, Arc<dyn TicketProvider>>,
    item: TicketSyncRecord,
) {
    let Some(provider) = providers.get(&item.provider) else {
        observer.log(
            ObserverCategory::TicketIntake,
            format!("Ticket sync skipped: missing provider {}", item.provider),
        );
        let _ = store.update_status(&item.dedup_key, "sync_skipped");
        return;
    };

    if !provider.supports_writeback() {
        let _ = store.update_status(&item.dedup_key, "sync_skipped");
        return;
    }

    let ticket = build_sync_ticket(&item);
    let message = format_sync_comment(&item);

    let comment_result = provider.post_comment(&ticket, &message).await;
    if let Err(e) = &comment_result {
        observer.log(
            ObserverCategory::TicketIntake,
            format!("Ticket sync comment failed: {}", e),
        );
    }

    let status_result = provider.update_status(&ticket, &item.task_status).await;
    if let Err(e) = &status_result {
        observer.log(
            ObserverCategory::TicketIntake,
            format!("Ticket sync status update failed: {}", e),
        );
    }

    let new_status = if comment_result.is_ok() && status_result.is_ok() {
        "synced"
    } else {
        "sync_failed"
    };
    let _ = store.update_status(&item.dedup_key, new_status);
}

fn build_sync_ticket(item: &TicketSyncRecord) -> ExternalTicket {
    ExternalTicket {
        external_id: item.external_id.clone(),
        number: item.issue_number.clone(),
        provider: item.provider.clone(),
        title: item.title.clone(),
        body: String::new(),
        labels: Vec::new(),
        priority: None,
        repo: repo_from_provider(&item.provider),
        url: String::new(),
        author: None,
    }
}

fn format_sync_comment(item: &crate::ticket_intake::store::TicketSyncRecord) -> String {
    let mut lines = vec![
        "Athena task update".to_string(),
        format!("Status: {}", item.task_status),
        format!("Goal: {}", truncate(&item.task_goal, 200)),
    ];

    if let Some(finished) = item.finished_at.as_ref() {
        lines.push(format!("Finished: {}", finished));
    }
    if let Some(err) = item.task_error.as_ref() {
        lines.push(format!("Error: {}", truncate(err, 200)));
    }

    lines.join("\n")
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        let mut end = max;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &value[..end])
    }
}

fn repo_from_provider(provider: &str) -> String {
    provider
        .split_once(':')
        .map(|(_, repo)| repo.to_string())
        .unwrap_or_default()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket_intake::store::TicketSyncRecord;

    fn make_record(status: &str, goal: &str) -> TicketSyncRecord {
        TicketSyncRecord {
            dedup_key: "key1".to_string(),
            provider: "github:owner/repo".to_string(),
            external_id: "42".to_string(),
            issue_number: Some("7".to_string()),
            title: "Fix bug".to_string(),
            task_status: status.to_string(),
            task_goal: goal.to_string(),
            task_error: None,
            finished_at: None,
        }
    }

    // --- truncate ---

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        let result = truncate("hello world", 5);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 8); // 5 chars + "..."
    }

    #[test]
    fn truncate_handles_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Multi-byte UTF-8: each char is 3 bytes
        let s = "áéíóú"; // 5 × 2-byte chars = 10 bytes
        let result = truncate(s, 3); // cut at byte 3
        // Must not panic and result must be valid UTF-8
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    // --- repo_from_provider ---

    #[test]
    fn repo_from_provider_extracts_repo_part() {
        assert_eq!(repo_from_provider("github:owner/repo"), "owner/repo");
        assert_eq!(repo_from_provider("linear:TEAM"), "TEAM");
    }

    #[test]
    fn repo_from_provider_returns_empty_when_no_colon() {
        assert_eq!(repo_from_provider("github"), "");
    }

    // --- format_sync_comment ---

    #[test]
    fn format_sync_comment_includes_status_and_goal() {
        let record = make_record("succeeded", "Fix the login bug");
        let comment = format_sync_comment(&record);
        assert!(comment.contains("Status: succeeded"));
        assert!(comment.contains("Fix the login bug"));
        assert!(comment.starts_with("Athena task update"));
    }

    #[test]
    fn format_sync_comment_includes_finished_at_when_set() {
        let mut record = make_record("succeeded", "Deploy feature");
        record.finished_at = Some("2026-02-28T12:00:00Z".to_string());
        let comment = format_sync_comment(&record);
        assert!(comment.contains("Finished: 2026-02-28T12:00:00Z"));
    }

    #[test]
    fn format_sync_comment_includes_error_when_failed() {
        let mut record = make_record("failed", "Deploy feature");
        record.task_error = Some("Build step failed".to_string());
        let comment = format_sync_comment(&record);
        assert!(comment.contains("Error: Build step failed"));
    }

    #[test]
    fn format_sync_comment_omits_optional_fields_when_absent() {
        let record = make_record("succeeded", "Deploy");
        let comment = format_sync_comment(&record);
        assert!(!comment.contains("Finished:"));
        assert!(!comment.contains("Error:"));
    }

    // --- build_sync_ticket ---

    #[test]
    fn build_sync_ticket_maps_fields_correctly() {
        let record = make_record("succeeded", "Deploy");
        let ticket = build_sync_ticket(&record);
        assert_eq!(ticket.external_id, "42");
        assert_eq!(ticket.number, Some("7".to_string()));
        assert_eq!(ticket.provider, "github:owner/repo");
        assert_eq!(ticket.repo, "owner/repo");
        assert_eq!(ticket.title, "Fix bug");
    }
}
