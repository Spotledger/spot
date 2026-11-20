//! The expiry sweeper and the start-up check of the occupancy service.
//!
//! # The sweeper
//!
//! A hold is `[start, end)` with a deadline; the exclusion constraint cannot see the clock, so an
//! expired hold keeps its row until something deletes it. The claim takes over an expired row in
//! its own transaction, so a booking is never wrong because the sweeper is late or absent. The
//! sweeper only reclaims rows nobody asks for and emits `occupancy_expired` for them.
//!
//! **Exactly one node sweeps per site**: the sweeper takes the consumer lease
//! `occupancy_sweeper` from the coordinator ([`OutboxLease`]) and a node that does not hold it
//! stands by and retries. The holder renews the lease at least every ten seconds, checks that it
//! still holds it before each pass, and releases it on stop. A pass is bounded (a batch of
//! `sweep_batch` rows, at most [`MAX_BATCHES`] batches), locks with `FOR UPDATE SKIP LOCKED`, and
//! deletes the rows together with their events in **one transaction**: a crash mid-pass rolls the
//! pass back and loses nothing, the next pass finds the same rows.
//!
//! The sweeper runs as the platform and touches only expired tentative rows of units this node
//! owns. A failed pass is logged (never with a value) and retried at the next interval.
//!
//! # The check
//!
//! At start the service verifies that the `btree_gist` extension exists and that the exclusion
//! constraint `ex_occupancy_during` exists on the table. Either missing keeps the claim routes
//! closed (`server.unavailable`) and degrades the site: without the constraint a claim could
//! double-book silently, so nothing is served on a guess.

use std::sync::Arc;
use std::time::Duration;

use spot_occupancy::Occupancy;
use spot_occupancy::declaration::EXCLUSION;
use spot_postgres::PgBackend;
use tokio::sync::watch;
use tokio::time::Instant;

use super::adapter::Ready;
use crate::services::ports::{ConsumerLease, OutboxLease};
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// The coordinator lease the sweeper holds.
pub const CONSUMER: &str = "occupancy_sweeper";
/// Batches one pass runs at most.
pub const MAX_BATCHES: u32 = 10;
/// The lease is renewed at least this often, whatever the sweep interval.
const RENEW_EVERY: Duration = Duration::from_secs(10);
/// How long a standby node waits before asking for the lease again.
const STANDBY: Duration = Duration::from_secs(10);

/// The occupancy service: the start-up check and the sweeper.
pub struct OccupancyService {
    pub(super) backend: PgBackend,
    pub(super) occupancy: Arc<Occupancy<PgBackend>>,
    pub(super) outbox: Arc<dyn OutboxLease>,
    pub(super) interval: Duration,
    pub(super) ready: Arc<Ready>,
}

/// Why the database cannot serve holds, or `None` when it can.
pub async fn missing(backend: &PgBackend) -> Result<Option<String>, String> {
    let client = backend
        .pool()
        .get()
        .await
        .map_err(|e| format!("the database is not reachable: {e}"))?;
    let row = client
        .query_opt(
            "SELECT 1 FROM pg_extension WHERE extname = 'btree_gist'",
            &[],
        )
        .await
        .map_err(|e| format!("cannot read the installed extensions: {e}"))?;
    if row.is_none() {
        return Ok(Some(
            "the `btree_gist` extension is not installed; install it with `extension.install` in the Admin Center database manager"
                .to_owned(),
        ));
    }
    let live = spot_pg_migrate::LiveSchema::read(backend)
        .await
        .map_err(|e| format!("cannot read the live schema: {e}"))?;
    // The database names the constraint `spot_<table>_<declared name>_excl` (ADR-087 section
    // 5.3), so the declared name is the middle of the physical one.
    let physical = format!("{EXCLUSION}_excl");
    if !live
        .constraints
        .iter()
        .any(|((_, name), c)| c.kind == 'x' && name.ends_with(&physical))
    {
        return Ok(Some(format!(
            "the exclusion constraint `{EXCLUSION}` does not exist; run `spot migrate` so the occupancy table has it"
        )));
    }
    Ok(None)
}

impl Service for OccupancyService {
    fn name(&self) -> &'static str {
        "occupancy"
    }

    fn level(&self) -> Level {
        // Bookings are an optional capability: a site that cannot hold serves everything else
        // and says it is degraded.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.ready.set(false);
            match missing(&self.backend).await? {
                None => {
                    self.ready.set(true);
                    Ok(())
                }
                Some(reason) => Err(reason),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if let Ok(lease) = self.outbox.acquire(CONSUMER).await
                    && self.lead(&mut stop, lease).await
                {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(STANDBY) => {}
                }
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        self.ready.set(false);
        Box::pin(async { Ok(()) })
    }
}

impl OccupancyService {
    /// One pass: batches until one is not full, at most [`MAX_BATCHES`]. Never panics; an error
    /// is logged by its fixed code.
    pub async fn sweep(&self) {
        match self.occupancy.sweep_all(MAX_BATCHES).await {
            Ok(report) => {
                if report.expired > 0 {
                    tracing::debug!(
                        expired = report.expired,
                        more = report.more,
                        "occupancy sweep"
                    );
                }
            }
            Err(e) => tracing::warn!(
                code = e.code(),
                "the occupancy sweep failed; the next pass retries"
            ),
        }
    }

    /// Sweeps as the lease holder; `true` means the node was stopped.
    async fn lead(&self, stop: &mut watch::Receiver<bool>, mut lease: ConsumerLease) -> bool {
        let tick = self.interval.min(RENEW_EVERY);
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first pass runs at once: a node that just took over finds what the last holder left.
        let mut last = Instant::now()
            .checked_sub(self.interval)
            .unwrap_or_else(Instant::now);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.outbox.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    if !matches!(self.outbox.still_held(&lease).await, Ok(true)) {
                        break false;
                    }
                    if last.elapsed() >= self.interval {
                        last = Instant::now();
                        self.sweep().await;
                    }
                }
            }
        };
        let _ = self.outbox.release(&lease).await;
        stopped
    }
}
