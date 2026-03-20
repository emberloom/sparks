use std::sync::Arc;
use tokio::sync::mpsc;

use crate::core::AutonomousTask;
use crate::knobs::SharedKnobs;
use crate::observer::{ObserverCategory, ObserverHandle};
use crate::randomness;

pub mod github;
pub mod gitlab;
pub mod jira;
pub mod linear;
pub mod provider;
pub mod store;
pub mod sync;
#[cfg(feature = "webhook")]
pub mod webhook;

pub use provider::TicketProvider;
pub use store::TicketIntakeStore;

pub fn spawn_ticket_intake(
    knobs: SharedKnobs,
    observer: ObserverHandle,
    auto_tx: mpsc::Sender<AutonomousTask>,
    providers: Vec<Arc<dyn TicketProvider>>,
    store: Arc<TicketIntakeStore>,
    inject_full_context: bool,
    rich_context_char_cap: usize,
) {
    if providers.is_empty() {
        observer.log(
            ObserverCategory::TicketIntake,
            "Ticket intake not started: no providers configured",
        );
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            let (enabled, interval) = {
                let k = knobs.read().unwrap_or_else(|e| e.into_inner());
                (k.ticket_intake_enabled, k.ticket_intake_interval_secs)
            };

            if !enabled {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let sleep_dur = randomness::jitter_interval(interval, 0.15);
            tokio::time::sleep(sleep_dur).await;

            {
                let k = knobs.read().unwrap_or_else(|e| e.into_inner());
                if !k.ticket_intake_enabled {
                    continue;
                }
            }

            for provider in providers.iter() {
                let name = provider.name();
                let tickets = match provider.poll().await {
                    Ok(t) => t,
                    Err(e) => {
                        observer.log(
                            ObserverCategory::TicketIntake,
                            format!("{} poll failed: {}", name, e),
                        );
                        continue;
                    }
                };

                if tickets.is_empty() {
                    continue;
                }

                let mut dispatched = 0usize;
                let mut skipped = 0usize;

                for ticket in tickets {
                    let dedup_key = ticket.dedup_key();
                    match store.is_seen(&dedup_key) {
                        Ok(true) => {
                            skipped += 1;
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            observer.log(
                                ObserverCategory::TicketIntake,
                                format!("{}: dedup lookup failed: {}", name, e),
                            );
                            continue;
                        }
                    }

                    if let Err(e) = store.mark_seen(
                        &dedup_key,
                        &ticket.provider,
                        &ticket.external_id,
                        ticket.number.as_deref(),
                        &ticket.title,
                    ) {
                        observer.log(
                            ObserverCategory::TicketIntake,
                            format!("{}: failed to mark seen: {}", name, e),
                        );
                        continue;
                    }

                    let mut task = ticket.to_autonomous_task();

                    if inject_full_context {
                        match provider.fetch_full_context(&ticket).await {
                            Ok(ctx) => {
                                let formatted = ctx.format(rich_context_char_cap);
                                if !formatted.is_empty() {
                                    task.context.push_str("\n\n### Full Ticket Context\n");
                                    task.context.push_str(&formatted);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "fetch_full_context failed, continuing without it: {}",
                                    e
                                );
                            }
                        }
                    }

                    match auto_tx.send(task).await {
                        Ok(_) => dispatched += 1,
                        Err(e) => {
                            observer.log(
                                ObserverCategory::TicketIntake,
                                format!("{}: dispatch failed: {}", name, e),
                            );
                        }
                    }
                }

                if dispatched > 0 || skipped > 0 {
                    observer.log(
                        ObserverCategory::TicketIntake,
                        format!("{}: dispatched {}, skipped {}", name, dispatched, skipped),
                    );
                }
            }
        }
    });
}
