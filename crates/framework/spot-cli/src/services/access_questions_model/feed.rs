//! The consumer that keeps the model current (WP-IAM-08.T15): it reads the site's outbox as the
//! lease holder of [`CONSUMER`], turns role, assignment and user changes into
//! [`AccessChange`](super::events::AccessChange)s, has the live source apply them, and writes the
//! change events they produce to the access-change journal.
//!
//! - **Where it starts.** A new lease starts at the end of the outbox: what happened before is in
//!   the stored records, which the first full read takes, so the model is complete from the start
//!   and the journal only gains events from now on. (A durable position would also journal the
//!   changes made while no node ran; see the report's open questions.)
//! - **When it says the view is broken.** A lease taken (another node may have changed things), a
//!   read that failed, a stream whose generation changed: the source is told to read everything.
//! - **Failure.** A journal that cannot take an event is logged and counted; the model is already
//!   updated, so the answers stay right and only the history misses that entry. The consumer never
//!   stops the site: it is an optional service.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spot_storage_contract::FeedPosition;
use tokio::sync::watch;

use super::events::{AccessChange, CONSUMER, classify};
use super::history::{self, ChangeJournal};
use super::live::LiveModelSource;
use crate::services::ports::{ConsumerLease, OutboxLease};
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// How long the consumer waits between passes.
pub const TICK: Duration = Duration::from_secs(1);

/// The service name.
pub const NAME: &str = "access_questions_feed";

/// How many outbox pages the start-up skip reads at most.
const SKIP_PAGES: u32 = 10_000;

/// What the consumer has done, as counts.
#[derive(Debug, Default)]
pub struct FeedStats {
    applied: AtomicU64,
    journaled: AtomicU64,
    journal_failures: AtomicU64,
    rebuilds: AtomicU64,
}

impl FeedStats {
    /// Access changes applied to the model.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Change events written to the journal.
    #[must_use]
    pub fn journaled(&self) -> u64 {
        self.journaled.load(Ordering::Relaxed)
    }

    /// Change events the journal refused.
    #[must_use]
    pub fn journal_failures(&self) -> u64 {
        self.journal_failures.load(Ordering::Relaxed)
    }

    /// Full rebuilds the consumer asked for.
    #[must_use]
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
    }
}

/// The access-change consumer of one site.
pub struct ChangeFeedService {
    outbox: Arc<dyn OutboxLease>,
    source: Arc<LiveModelSource>,
    journal: Option<Arc<dyn ChangeJournal>>,
    batch: u32,
    tick: Duration,
    stats: Arc<FeedStats>,
}

impl ChangeFeedService {
    /// A consumer over `outbox` that keeps `source` current and journals through the source's
    /// journal. Attaches itself to the source (it then keeps its cache for the repair interval).
    #[must_use]
    pub fn new(outbox: Arc<dyn OutboxLease>, source: Arc<LiveModelSource>) -> Self {
        source.attach_feed();
        let journal = source.journal().cloned();
        Self {
            outbox,
            source,
            journal,
            batch: 100,
            tick: TICK,
            stats: Arc::new(FeedStats::default()),
        }
    }

    /// The same consumer with a different pass interval (tests).
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// The counts.
    #[must_use]
    pub fn stats(&self) -> Arc<FeedStats> {
        Arc::clone(&self.stats)
    }

    fn rebuild(&self) {
        self.source.request_rebuild();
        self.stats.rebuilds.fetch_add(1, Ordering::Relaxed);
    }

    /// The end of the outbox: where a new lease starts.
    ///
    /// # Errors
    ///
    /// The outbox could not be read.
    pub async fn end_of_outbox(&self) -> Result<FeedPosition, crate::services::ports::PortError> {
        let mut position = FeedPosition::START;
        for _ in 0..SKIP_PAGES {
            let page = self.outbox.read(position, self.batch).await?;
            let Some(last) = page.last() else {
                break;
            };
            position = last.position;
            if page.len() < usize::try_from(self.batch).unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(position)
    }

    /// One pass: reads the page after `after`, applies the access changes in it, journals their
    /// events and returns the position to continue from and how many changes it applied.
    ///
    /// # Errors
    ///
    /// The outbox could not be read; the position does not move and the source is told to read
    /// everything.
    pub async fn pump(
        &self,
        after: FeedPosition,
    ) -> Result<(FeedPosition, usize), crate::services::ports::PortError> {
        let page = match self.outbox.read(after, self.batch).await {
            Ok(page) => page,
            Err(error) => {
                self.rebuild();
                return Err(error);
            }
        };
        let Some(last) = page.last() else {
            return Ok((after, 0));
        };
        if after != FeedPosition::START
            && page
                .first()
                .is_some_and(|r| r.position.generation() != after.generation())
        {
            // The stream restarted (a restore): the stored records are the truth.
            self.rebuild();
        }
        let next = last.position;
        let changes: Vec<AccessChange> = page.iter().filter_map(classify).collect();
        if changes.is_empty() {
            return Ok((next, 0));
        }
        match self.source.apply_changes(&changes).await {
            Ok(events) => {
                self.stats
                    .applied
                    .fetch_add(changes.len() as u64, Ordering::Relaxed);
                self.journal_all(&events).await;
            }
            Err(error) => {
                // The changes stay noted in the source: the next question applies them.
                tracing::warn!(
                    code = error.code(),
                    "access changes could not be applied yet; the next question retries"
                );
            }
        }
        Ok((next, changes.len()))
    }

    async fn journal_all(&self, events: &[spot_iam_governance::ChangeEvent]) {
        let Some(journal) = &self.journal else {
            return;
        };
        for event in events {
            match history::record(journal.as_ref(), event).await {
                Ok(_) => {
                    self.stats.journaled.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.stats.journal_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("an access change could not be written to the journal");
                }
            }
        }
    }

    /// Runs as the lease holder; `true` means the site is stopping.
    async fn lead(&self, stop: &mut watch::Receiver<bool>, mut lease: ConsumerLease) -> bool {
        // Another node may have held the lease before: read everything once.
        self.rebuild();
        let mut position = match self.end_of_outbox().await {
            Ok(position) => position,
            Err(_) => {
                let _ = self.outbox.release(&lease).await;
                return false;
            }
        };
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                    // Drain what is there, a page at a time.
                    while let Ok((next, seen)) = self.pump(position).await {
                        position = next;
                        if seen < usize::try_from(self.batch).unwrap_or(usize::MAX) {
                            break;
                        }
                    }
                }
            }
        };
        let _ = self.outbox.release(&lease).await;
        stopped
    }
}

impl Service for ChangeFeedService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        // The questions answer without it, from the timer's full reads.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { Ok(()) })
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
                    () = tokio::time::sleep(self.tick) => {}
                }
            }
        })
    }
}
