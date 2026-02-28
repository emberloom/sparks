use std::collections::HashMap;
use std::sync::Arc;

use tokio::time::{Duration, MissedTickBehavior};

use crate::knobs::SharedKnobs;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::ticket_intake::provider::ExternalTicket;
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

            let poll_enabled = {
                let k = knobs.read().unwrap();
                k.ticket_intake_enabled
            };
            if !poll_enabled && !webhook_enabled {
                continue;
            }

            let pending = match store.get_pending_syncs(25) {
                Ok(rows) => rows,
                Err(e) => {
                    observer.log(
                        ObserverCategory::TicketIntake,
                        format!("Ticket sync query failed: {}", e),
                    );
                    continue;
                }
            };

            if pending.is_empty() {
                continue;
            }

            for item in pending {
                let Some(provider) = providers.get(&item.provider) else {
                    observer.log(
                        ObserverCategory::TicketIntake,
                        format!("Ticket sync skipped: missing provider {}", item.provider),
                    );
                    let _ = store.update_status(&item.dedup_key, "sync_skipped");
                    continue;
                };

                if !provider.supports_writeback() {
                    let _ = store.update_status(&item.dedup_key, "sync_skipped");
                    continue;
                }

                let ticket = ExternalTicket {
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
                };

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
        }
    });
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
