//! The SLA service and the calendar notice.

use std::sync::Arc;
use std::time::Duration;

use spot_bpm_tasks::{TaskId, TaskStore};
use spot_pg_bridge::Bridge;
use tokio::sync::watch;

use super::ops::Core;
use super::threads::plain;
use super::{LEASE_CONSUMER, REARM_EVERY_TICKS};
use crate::services::ports::{ConsumerLease, OutboxLease};
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// Holds the SLA lease, re-arms the timers and fires the steps that are due.
pub(super) struct SlaService {
    pub(super) core: Arc<Core>,
    pub(super) bridge: Arc<Bridge>,
    pub(super) outbox: Arc<dyn OutboxLease>,
    pub(super) interval: Duration,
}

impl Service for SlaService {
    fn name(&self) -> &'static str {
        "process_tasks"
    }

    fn level(&self) -> Level {
        // A broken inbox is not a reason to take document serving down.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let core = Arc::clone(&self.core);
            let probe = plain(move || {
                let id = TaskId::new("health_probe");
                match id {
                    Some(id) => core.store.load(&id).map(|_| ()),
                    None => Ok(()),
                }
            })
            .await;
            match probe {
                Some(Ok(())) => Ok(()),
                Some(Err(error)) => Err(format!(
                    "the task store is not usable ({}); run `spot migrate`",
                    error.code()
                )),
                None => Err("the task store check could not run".to_owned()),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let standby = (self.interval / 3).max(Duration::from_millis(500));
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                match self.outbox.acquire(LEASE_CONSUMER).await {
                    Ok(lease) => {
                        if self.lead(&mut stop, &lease).await {
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        tracing::debug!("the task SLA lease is held elsewhere; standing by");
                    }
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(standby) => {}
                }
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            let bridge = Arc::clone(&self.bridge);
            // The bridge blocks while it drains: never on a runtime thread.
            match plain(move || bridge.shutdown(Duration::from_secs(5)).is_ok()).await {
                Some(true) | None => Ok(()),
                Some(false) => Err("the task store bridge did not drain in time".to_owned()),
            }
        })
    }
}

impl SlaService {
    /// Leads the SLA loop while the lease is held. `true`: the node was stopped. `false`: the
    /// lease was lost (the caller tries to take it again).
    async fn lead(&self, stop: &mut watch::Receiver<bool>, lease: &ConsumerLease) -> bool {
        let host_core = Arc::clone(&self.core);
        match plain(move || host_core.rearm()).await {
            Some(Ok(armed)) => tracing::debug!(armed, "task SLA timers armed from the store"),
            Some(Err(error)) => {
                tracing::warn!(code = error.code(), "arming the task SLA timers failed");
            }
            None => tracing::warn!("the task SLA re-arm thread did not run"),
        }
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut count: u64 = 0;
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.outbox.still_held(lease).await {
                        Ok(true) => {
                            count += 1;
                            let core = Arc::clone(&self.core);
                            let now = core.now();
                            let again = count.is_multiple_of(REARM_EVERY_TICKS);
                            let outcome = plain(move || {
                                if again {
                                    let _ = core.rearm();
                                }
                                core.tick(now)
                            })
                            .await;
                            match outcome {
                                Some(Ok(_)) => {}
                                Some(Err(error)) => {
                                    tracing::warn!(code = error.code(), "a task SLA check failed");
                                }
                                None => tracing::warn!("the task SLA thread did not run"),
                            }
                        }
                        Ok(false) | Err(_) => break false,
                    }
                }
            }
        };
        let _ = self.outbox.release(lease).await;
        stopped
    }
}

/// Says in readiness that SLAs run on wall time. It checks as failed on purpose: an optional
/// service that fails its check only degrades readiness with this sentence, and it does nothing
/// else, so no SLA is held back by it.
pub(super) struct CalendarNotice;

impl Service for CalendarNotice {
    fn name(&self) -> &'static str {
        "process_tasks_calendar"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async {
            Err(
                "task SLAs run on wall time: no [services.process_tasks.calendar] is configured"
                    .to_owned(),
            )
        })
    }

    fn run(&self, _stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}
