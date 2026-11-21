//! The indexer's loop: one node per site holds the `retrieval_indexer` lease and pumps the change
//! feed into the vector store from a durable cursor.
//!
//! - **One indexer per site.** The loop takes the consumer lease from the coordinator
//!   ([`OutboxLease`]); a node that does not hold it stands by and asks again. The holder renews
//!   the lease on every tick, checks that it still holds it before a pass, and releases it on stop.
//! - **From a cursor.** `spot_retrieval::Indexer::pump_once` reads committed changes after the
//!   stored position and moves the position only after the store accepted the batch. A crash
//!   between the two replays a range, and the store replaces by `(document, version)`, so a replay
//!   adds no duplicate and an older version never overwrites a newer one.
//! - **Default deny.** A DocType with no entry in the index policy is not indexed. A field the
//!   permission rules protect is left out of the index (and so out of every embedding request)
//!   unless the policy lists it and the tenant allows protected fields.
//! - **Failures delay, never lose.** A feed, embedding or store failure leaves the cursor where it
//!   was; the loop backs off and retries. A document that cannot be read keeps the cursor too
//!   ([`super::ports::GatedPositions`]). The loop logs a code and counts, never a text or a value.
//! - **Backfill.** `platform:retrieval.backfill` asks the holder to reset the cursor to the start
//!   of the feed and replay it (after a policy or a protection change, or an embedding model
//!   change with a new index).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use spot_core::{Clock, SiteId};
use spot_field_guard::ProtectionSource;
use spot_retrieval::{
    Indexer, IndexerConfig, PositionStore, PumpReport, RetrievalError, VectorStore,
};
use spot_storage_contract::ChangeFeed;
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::Settings;
use super::access::{PolicyError, PolicySource};
use super::ports::{
    BlockingEmbedder, EmbedPort, EmbedPurpose, FilePositions, GatedPositions, SystemDocuments,
};
use crate::services::ports::{ConsumerLease, OutboxLease, SystemReader};

/// The coordinator lease the indexer holds.
pub const INDEXER_LEASE: &str = "retrieval_indexer";

/// How long a standby node waits before asking for the lease again.
const STANDBY: Duration = Duration::from_secs(10);

/// The lease is renewed at least this often, whatever the pump interval.
const RENEW_EVERY: Duration = Duration::from_secs(10);

/// The most full pages one tick runs back to back before it lets the lease and the stop signal
/// be looked at again.
const MAX_PAGES_PER_TICK: u32 = 20;

/// What the indexer has done, for readiness and tests. Counts only: nothing here names a document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Whether this node holds the lease and pumps.
    pub leader: bool,
    /// Documents indexed since the node started.
    pub indexed: u64,
    /// Documents whose chunks were removed since the node started.
    pub deleted: u64,
    /// Documents skipped (too large, or the provider refused them) since the node started.
    pub skipped: u64,
    /// The stable code of the last failed pass, cleared by the next good one.
    pub last_error: Option<&'static str>,
    /// Why the policy is not applied, when it is not.
    pub policy_problem: Option<&'static str>,
}

/// State the service, the job handler and the loop share.
#[derive(Default)]
pub struct Shared {
    status: Mutex<Status>,
    /// Whether this node holds the indexer lease.
    pub leader: AtomicBool,
    /// A backfill was asked for; the holder resets the cursor at its next tick.
    pub backfill: AtomicBool,
    /// Whether the start-up check passed (the store is prepared).
    pub ready: Arc<AtomicBool>,
    /// Passes run since the node started.
    pub passes: AtomicU64,
}

impl Shared {
    /// A fresh, not-ready state.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A copy of the status.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        change(&mut status);
    }

    /// Why the site is degraded right now, without a key, an address or a value.
    #[must_use]
    pub fn degraded(&self) -> Option<String> {
        let status = self.status();
        if let Some(code) = status.policy_problem {
            return Some(format!("{code}: the index policy is not applied"));
        }
        status
            .last_error
            .map(|code| format!("{code}: the last indexing pass failed and will be retried"))
    }
}

/// What the loop is built from.
pub struct PumpDeps<F: ChangeFeed> {
    /// The site.
    pub site: SiteId,
    /// The change feed (the site's backend).
    pub feed: F,
    /// The coordinator lease.
    pub lease: Arc<dyn OutboxLease>,
    /// The platform's document reader.
    pub reader: Arc<dyn SystemReader>,
    /// The embedding port the LLM service provides.
    pub embed: Arc<dyn EmbedPort>,
    /// The vector store.
    pub store: Arc<dyn VectorStore>,
    /// The durable cursor.
    pub cursor: Arc<FilePositions>,
    /// The protection facts of the metadata served now.
    pub protection: Arc<dyn ProtectionSource>,
    /// The index policy source.
    pub policy: Arc<dyn PolicySource>,
    /// The site clock (the only time the indexer sees).
    pub clock: Arc<dyn Clock>,
    /// The settings.
    pub settings: Settings,
    /// The shared state.
    pub shared: Arc<Shared>,
}

/// The loop.
pub struct Pump<F: ChangeFeed> {
    deps: PumpDeps<F>,
}

impl<F: ChangeFeed + Send + Sync + 'static> Pump<F> {
    /// A loop over `deps`.
    #[must_use]
    pub fn new(deps: PumpDeps<F>) -> Self {
        Self { deps }
    }

    /// Builds the indexer, or says why the policy is not ready.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] from the policy source.
    pub fn indexer(&self, handle: Handle) -> Result<Indexer, PolicyError> {
        let deps = &self.deps;
        let policy = deps.policy.policy()?;
        let failed = Arc::new(AtomicBool::new(false));
        let documents = Arc::new(SystemDocuments::new(
            Arc::clone(&deps.reader),
            handle.clone(),
            Arc::clone(&failed),
        ));
        let embeddings = Arc::new(BlockingEmbedder::new(
            Arc::clone(&deps.embed),
            &deps.settings.provider,
            EmbedPurpose::Index,
            None,
            deps.settings.dimensions,
            deps.settings.batch_size,
            handle,
        ));
        let positions: Arc<dyn PositionStore> = Arc::new(GatedPositions::new(
            Arc::clone(&deps.cursor) as Arc<dyn PositionStore>,
            failed,
        ));
        Ok(Indexer::new(
            deps.site.clone(),
            policy,
            documents,
            embeddings,
            Arc::clone(&deps.store),
            positions,
            IndexerConfig {
                max_document_chars: deps.settings.max_document_chars,
                embed_batch: deps.settings.batch_size,
            },
        )
        .with_protection(Arc::clone(&deps.protection), deps.settings.index_protected))
    }

    /// One pump pass: up to `page` committed changes after the cursor.
    ///
    /// # Errors
    ///
    /// The indexer's [`RetrievalError`]; the cursor has not moved.
    pub async fn pass(&self, indexer: &mut Indexer) -> Result<PumpReport, RetrievalError> {
        let now = SystemTime::from(self.deps.clock.now_utc());
        let report = indexer
            .pump_once(&self.deps.feed, self.deps.settings.page, now)
            .await?;
        self.deps.shared.passes.fetch_add(1, Ordering::SeqCst);
        self.deps.shared.update(|status| {
            status.indexed += u64::from(report.indexed);
            status.deleted += u64::from(report.deleted);
            status.skipped += u64::try_from(report.skipped.len()).unwrap_or(u64::MAX);
            status.last_error = None;
        });
        Ok(report)
    }

    /// Runs until `stop` is set: take the lease, lead while it holds, stand by otherwise.
    ///
    /// # Errors
    ///
    /// Never; the signature matches `Service::run`.
    pub async fn run(&self, stop: watch::Receiver<bool>) -> Result<(), String> {
        let mut stop = stop;
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            if let Ok(lease) = self.deps.lease.acquire(INDEXER_LEASE).await
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
    }

    /// Pumps as the lease holder; `true` means the node was stopped.
    async fn lead(&self, stop: &mut watch::Receiver<bool>, mut lease: ConsumerLease) -> bool {
        let shared = &self.deps.shared;
        shared.leader.store(true, Ordering::SeqCst);
        shared.update(|status| status.leader = true);
        let interval = Duration::from_millis(self.deps.settings.interval_ms);
        let mut ticker = tokio::time::interval(interval.min(RENEW_EVERY));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut indexer: Option<Indexer> = None;
        let mut failures = 0u32;
        let mut last = tokio::time::Instant::now()
            .checked_sub(interval)
            .unwrap_or_else(tokio::time::Instant::now);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.deps.lease.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    if !matches!(self.deps.lease.still_held(&lease).await, Ok(true)) {
                        break false;
                    }
                    let wait = if failures == 0 {
                        interval
                    } else {
                        Duration::from_secs(u64::from(1u32 << failures.min(6)).min(60))
                    };
                    if last.elapsed() < wait {
                        continue;
                    }
                    last = tokio::time::Instant::now();
                    if shared.backfill.swap(false, Ordering::SeqCst) {
                        match self.deps.cursor.reset() {
                            Ok(()) => tracing::info!(
                                target: "spot::retrieval",
                                site = %self.deps.site.as_str(),
                                "retrieval backfill: the cursor was reset to the start of the feed"
                            ),
                            Err(error) => {
                                tracing::warn!(
                                    target: "spot::retrieval",
                                    code = error.code(),
                                    "retrieval backfill: the cursor could not be reset"
                                );
                                shared.backfill.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    failures = self.tick(&mut indexer, failures, stop).await;
                }
            }
        };
        shared.leader.store(false, Ordering::SeqCst);
        shared.update(|status| status.leader = false);
        let _ = self.deps.lease.release(&lease).await;
        stopped
    }

    /// One tick: build the indexer if it is not built, then pump full pages back to back.
    /// Returns the new consecutive failure count.
    async fn tick(
        &self,
        indexer: &mut Option<Indexer>,
        failures: u32,
        stop: &watch::Receiver<bool>,
    ) -> u32 {
        let shared = &self.deps.shared;
        if indexer.is_none() {
            match self.indexer(Handle::current()) {
                Ok(built) => {
                    shared.update(|status| status.policy_problem = None);
                    *indexer = Some(built);
                }
                Err(PolicyError::NotReady) => return failures,
                Err(PolicyError::Invalid(error)) => {
                    let code = error.code();
                    shared.update(|status| status.policy_problem = Some(code));
                    tracing::warn!(
                        target: "spot::retrieval",
                        site = %self.deps.site.as_str(),
                        code,
                        "the index policy is refused; nothing is indexed until it is fixed"
                    );
                    return failures.saturating_add(1);
                }
            }
        }
        let Some(active) = indexer.as_mut() else {
            return failures;
        };
        for _ in 0..MAX_PAGES_PER_TICK {
            match self.pass(active).await {
                Ok(report) => {
                    if report.read < self.deps.settings.page || *stop.borrow() {
                        return 0;
                    }
                }
                Err(error) => {
                    let code = error.code();
                    shared.update(|status| status.last_error = Some(code));
                    tracing::warn!(
                        target: "spot::retrieval",
                        site = %self.deps.site.as_str(),
                        code,
                        "an indexing pass failed; the cursor did not move and the pass is retried"
                    );
                    return failures.saturating_add(1);
                }
            }
        }
        0
    }
}
