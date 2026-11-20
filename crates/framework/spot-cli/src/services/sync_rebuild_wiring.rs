//! The sync and rebuild wiring: the production providers that process distribution and the
//! offline rebuild host were written without (WP-RTB-01.T10, ADR-061 section 6, ADR-062). Operator
//! guide: `docs/running-spot/sync_rebuild_wiring.md`.
//!
//! # Site services (registered by this module)
//!
//! `sync_process` (WP-SYN-12.T2) asks for two ports while it registers; this module (ORDER 1235,
//! before its 1240) provides them:
//!
//! - **`PinSource`** ([`CachedPins`]): the number of running BPM Cases pinned to `(process,
//!   version)`, counted from `spot_bpm_case` by a refresher every few seconds. The answer is read
//!   from memory; **a count older than `pin_max_age_seconds` (or never read) says "one pinned"**,
//!   so nothing is ever retired on a guess.
//! - **`HubStream`** ([`QueuedHubStream`]): `emit` queues the hub-authored process record with its
//!   scope keys and a flusher hands it to the hub sync service's [`MasterRecordSink`] port, in
//!   order, retrying with backoff. Provided **only on a hub or standalone role and only when the
//!   hub sync service provides the sink**; otherwise the hub side of process distribution stays
//!   off (fail closed). BPM edge definitions are read through `ProcessSync` from
//!   `spot_sync_process_definition`: the BPM store assigns versions itself, so hub versions are
//!   not installed there.
//!
//! # Rebuild providers (built by the CLI, which has no running site)
//!
//! [`StoreRebuilder`] (the executor over the read-only journal store, with the reveal of protected
//! fields), [`StoreJournal`] (the catalogue from verified manifests and snapshots, the fork
//! through the relay), [`LeaseGuard`] (refuses while the shipper's lease is live and takes a
//! maintenance lease for the run) and [`InvariantSuite`] (a runner over [`InvariantCheck`]s that
//! **fails closed with no checks registered**). The production checks are owed (see the task
//! report).
//!
//! Settings `[services.sync_rebuild_wiring]`: `enabled`, `pin_refresh_seconds` (5),
//! `pin_max_age_seconds` (30), `queue_capacity` (1000). Unknown keys are refused.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use spot_coordinator::{Coordinator, Lease};
use spot_core::Clock;
use spot_journal_manifest::{Fork, VerifyingKeys};
use spot_journal_protect::{RevealSink, SharedFields, SharedKeys};
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, RebuildKeys, RebuildOptions,
    SinkFuture,
};
use spot_journal_rebuild_host::{
    Fut, HostError, InvariantResult, Invariants, JournalSide, RebuildFut, Rebuilder, SiteGuard,
};
use spot_journal_rebuild_plan::{ForkPoint, JournalCatalog, RebuildPlan};
use spot_journal_relay::{Durability, Relay, RelayConfig};
use spot_journal_snapshot::{SnapshotKeys, list_snapshots};
use spot_journal_store::{FolderStore, JournalStore, TenantId};
use spot_postgres::PgBackend;
use spot_sync_process::ProcessRecord;
use tokio::sync::watch;

use crate::services::SiteCtx;
use crate::services::journal::{JournalKeys, TenantScoped, check_folder_target};
use crate::services::journal_wiring::{FolderReader, read_catalogue};
use crate::services::node_role::{ActiveRole, Role};
use crate::services::sync_process::{HubStream, PinSource};
use crate::services::{CheckFuture, Level, RunFuture, Service};
use crate::{Error, Result};

/// Start order: after `node_role` (1005), the hub sync hosting and the BPM runtime, before
/// `sync_process` (1240), which asks for the two ports while it registers.
pub const ORDER: u32 = 1235;

/// `[services.sync_rebuild_wiring]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` provides nothing: process distribution runs without pins (nothing is retired) and
    /// without a hub side.
    pub enabled: bool,
    /// How often the pinned-Case counts are read, in seconds.
    pub pin_refresh_seconds: u64,
    /// How old a count may be before it is no longer believed, in seconds.
    pub pin_max_age_seconds: u64,
    /// How many hub records may wait for the hub sync service.
    pub queue_capacity: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            pin_refresh_seconds: 5,
            pin_max_age_seconds: 30,
            queue_capacity: 1000,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn millis(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

// Pins.

type Counts = HashMap<(String, u64), u64>;

/// The pinned-Case counts, read from memory. Never believes an old or missing count.
pub struct CachedPins {
    clock: Arc<dyn Clock>,
    max_age_millis: i64,
    cache: Mutex<Option<(i64, Counts)>>,
}

impl CachedPins {
    /// An empty cache: every version reads as pinned until the first refresh.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, max_age: Duration) -> Self {
        Self {
            clock,
            max_age_millis: i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX),
            cache: Mutex::new(None),
        }
    }

    /// Replaces the counts, read at `at_millis`.
    pub fn store(&self, at_millis: i64, counts: Counts) {
        *lock(&self.cache) = Some((at_millis, counts));
    }
}

impl PinSource for CachedPins {
    fn pins(&self, process: &str, version: u64) -> u64 {
        let now = millis(self.clock.as_ref());
        match &*lock(&self.cache) {
            Some((at, counts)) if now.saturating_sub(*at) <= self.max_age_millis => counts
                .get(&(process.to_owned(), version))
                .copied()
                .unwrap_or(0),
            // Unknown or stale: one Case counts as pinned, so nothing is retired.
            _ => 1,
        }
    }
}

/// Reads the counts of running Cases per `(process, version)` for `tenant`.
async fn read_pins(backend: &PgBackend, tenant: &str) -> std::result::Result<Counts, ()> {
    let conn = backend.pool().get().await.map_err(|_| ())?;
    let rows = conn
        .query(
            "SELECT dag_key, dag_version::bigint, count(*)::bigint FROM spot_bpm_case \
             WHERE tenant = $1 AND status = 'running' GROUP BY dag_key, dag_version",
            &[&tenant],
        )
        .await
        .map_err(|_| ())?;
    let mut counts = Counts::new();
    for row in rows {
        let key: String = row.try_get(0).map_err(|_| ())?;
        let version: i64 = row.try_get(1).map_err(|_| ())?;
        let count: i64 = row.try_get(2).map_err(|_| ())?;
        counts.insert(
            (key, u64::try_from(version).map_err(|_| ())?),
            u64::try_from(count).map_err(|_| ())?,
        );
    }
    Ok(counts)
}

/// Keeps [`CachedPins`] fresh.
struct PinRefresher {
    backend: PgBackend,
    tenant: String,
    pins: Arc<CachedPins>,
    clock: Arc<dyn Clock>,
    every: Duration,
}

impl Service for PinRefresher {
    fn name(&self) -> &'static str {
        "sync_pins"
    }
    fn level(&self) -> Level {
        Level::Optional
    }
    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            read_pins(&self.backend, &self.tenant)
                .await
                .map(|_| ())
                .map_err(|()| {
                    "sync_rebuild_wiring.pins_unreadable: the BPM case table cannot be read, so no process version is retired".to_owned()
                })
        })
    }
    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut failing = false;
            loop {
                match read_pins(&self.backend, &self.tenant).await {
                    Ok(counts) => {
                        self.pins.store(millis(self.clock.as_ref()), counts);
                        failing = false;
                    }
                    Err(()) if !failing => {
                        failing = true;
                        tracing::warn!(
                            code = "sync_rebuild_wiring.pins_unreadable",
                            "sync_pins: the pinned Cases cannot be counted; nothing is retired"
                        );
                    }
                    Err(()) => {}
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(self.every) => {}
                }
            }
        })
    }
}

// The hub stream.

/// What the hub sync service provides to carry a hub-authored master record to the nodes in
/// scope: it writes the record to the hub's stream and registers its scope keys.
pub trait MasterRecordSink: Send + Sync {
    /// Writes `record` requiring all of `scope_keys` (empty: every subscriber).
    ///
    /// # Errors
    /// A sentence without the definition; the record is kept and retried.
    fn write<'a>(
        &'a self,
        record: &'a ProcessRecord,
        scope_keys: &'a [String],
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send + 'a>>;
}

type Queued = (ProcessRecord, Vec<String>);

/// The hub stream as the process distribution sees it: `emit` queues, the flusher delivers.
pub struct QueuedHubStream {
    queue: Mutex<VecDeque<Queued>>,
    capacity: usize,
}

impl QueuedHubStream {
    /// A stream that holds at most `capacity` records.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    /// How many records wait.
    #[must_use]
    pub fn waiting(&self) -> usize {
        lock(&self.queue).len()
    }

    fn front(&self) -> Option<Queued> {
        lock(&self.queue).front().cloned()
    }

    fn done(&self) {
        lock(&self.queue).pop_front();
    }
}

impl HubStream for QueuedHubStream {
    fn emit(
        &self,
        record: &ProcessRecord,
        scope_keys: &[String],
    ) -> std::result::Result<(), String> {
        let mut queue = lock(&self.queue);
        if queue.len() >= self.capacity {
            return Err("sync_rebuild_wiring.hub_queue_full".to_owned());
        }
        queue.push_back((record.clone(), scope_keys.to_vec()));
        Ok(())
    }
}

/// Delivers the queued records to the hub sync service, oldest first, one at a time.
struct HubFlusher {
    stream: Arc<QueuedHubStream>,
    sink: Arc<dyn MasterRecordSink>,
}

impl Service for HubFlusher {
    fn name(&self) -> &'static str {
        "sync_hub_stream"
    }
    fn level(&self) -> Level {
        Level::Optional
    }
    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }
    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut backoff = Duration::from_millis(250);
            loop {
                let pause = if let Some((record, keys)) = self.stream.front() {
                    match self.sink.write(&record, &keys).await {
                        Ok(()) => {
                            self.stream.done();
                            backoff = Duration::from_millis(250);
                            Duration::ZERO
                        }
                        Err(_) => {
                            tracing::warn!(
                                code = "sync_rebuild_wiring.hub_sink_refused",
                                waiting = self.stream.waiting(),
                                "sync_hub_stream: the hub sync service refused a record; retrying"
                            );
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                            backoff
                        }
                    }
                } else {
                    Duration::from_millis(250)
                };
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(pause) => {}
                }
            }
        })
    }
}

// Registration.

struct Provided {
    pins: Arc<CachedPins>,
    hub: Option<Arc<QueuedHubStream>>,
}

fn built() -> &'static Mutex<HashMap<String, Arc<Provided>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<Provided>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

fn provide(ctx: &mut SiteCtx<'_>, provided: &Provided) -> Result<()> {
    if ctx.port::<dyn PinSource>().is_none() {
        ctx.provide::<dyn PinSource>(Arc::clone(&provided.pins) as Arc<dyn PinSource>)?;
    }
    if let Some(hub) = &provided.hub
        && ctx.port::<dyn HubStream>().is_none()
    {
        ctx.provide::<dyn HubStream>(Arc::clone(hub) as Arc<dyn HubStream>)?;
    }
    Ok(())
}

/// Registers the pin source, and the hub stream on a hub that has the sync service.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sync_rebuild_wiring")?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = lock(built()).get(&site).cloned();
        if let Some(provided) = existing {
            provide(ctx, &provided)?;
        }
        return Ok(());
    }
    if settings.pin_refresh_seconds == 0 || settings.queue_capacity == 0 {
        return Err(Error(
            "services.sync_rebuild_wiring: pin_refresh_seconds and queue_capacity must be positive"
                .to_owned(),
        ));
    }
    let env = *ctx.env()?;
    let clock = Arc::clone(ctx.clock());
    let pins = Arc::new(CachedPins::new(
        Arc::clone(&clock),
        Duration::from_secs(settings.pin_max_age_seconds),
    ));
    ctx.add(Arc::new(PinRefresher {
        backend: env.backend.clone(),
        tenant: site.clone(),
        pins: Arc::clone(&pins),
        clock,
        every: Duration::from_secs(settings.pin_refresh_seconds),
    }));
    // A hub-authored record is emitted on a hub (or the standalone site that is its own hub),
    // and only when the hub sync service provides the sink.
    let on_hub = ctx
        .port::<ActiveRole>()
        .is_none_or(|active| matches!(active.role(), Role::Hub | Role::Standalone));
    let hub = match (on_hub, ctx.port::<dyn MasterRecordSink>()) {
        (true, Some(sink)) => {
            let stream = Arc::new(QueuedHubStream::new(settings.queue_capacity));
            ctx.add(Arc::new(HubFlusher {
                stream: Arc::clone(&stream),
                sink,
            }));
            Some(stream)
        }
        (true, None) => {
            tracing::debug!(
                code = "sync_rebuild_wiring.hub_sink_absent",
                "sync_rebuild_wiring: no hub sync service; the hub side of process distribution stays off"
            );
            None
        }
        (false, _) => None,
    };
    let provided = Arc::new(Provided { pins, hub });
    provide(ctx, &provided)?;
    lock(built()).insert(site, provided);
    Ok(())
}

// The rebuild providers.

/// A refusal of the invariant suite or a check that could not run.
fn failed(name: &str) -> InvariantResult {
    InvariantResult {
        name: name.to_owned(),
        held: false,
        checked: 0,
        violations: 0,
    }
}

/// One invariant the suite runs after the projectors (`document_counts`, `graph_edges`,
/// `financial_fingerprint`).
pub trait InvariantCheck: Send + Sync {
    /// Runs the check.
    fn run(&self) -> Fut<'_, InvariantResult>;
}

/// The invariant suite: every registered check, in order. **No checks is a failure**
/// (`no_invariant_registered`): a rebuild that was never checked is not reported as sound.
pub struct InvariantSuite {
    checks: Vec<Arc<dyn InvariantCheck>>,
}

impl InvariantSuite {
    /// A suite over `checks`.
    #[must_use]
    pub fn new(checks: Vec<Arc<dyn InvariantCheck>>) -> Self {
        Self { checks }
    }
}

impl Invariants for InvariantSuite {
    fn run(&self) -> Fut<'_, Vec<InvariantResult>> {
        Box::pin(async move {
            if self.checks.is_empty() {
                return Ok(vec![failed("sync_rebuild_wiring.no_invariant_registered")]);
            }
            let mut results = Vec::with_capacity(self.checks.len());
            for check in &self.checks {
                results.push(check.run().await?);
            }
            Ok(results)
        })
    }
}

/// Refuses while the shipper's writer lease is live (a node is running) and takes a maintenance
/// lease for the run. Fail closed: a coordinator that cannot answer is "running".
pub struct LeaseGuard {
    coordinator: Arc<dyn Coordinator>,
    holder: String,
    ttl: Duration,
    held: Mutex<Option<Lease>>,
}

impl LeaseGuard {
    /// A guard acting as `holder`, holding the maintenance lease for `ttl`.
    #[must_use]
    pub fn new(coordinator: Arc<dyn Coordinator>, holder: String, ttl: Duration) -> Self {
        Self {
            coordinator,
            holder,
            ttl,
            held: Mutex::new(None),
        }
    }

    /// Extends the maintenance lease (call between long steps).
    ///
    /// # Errors
    /// [`HostError::SiteRunning`] when the lease was lost.
    pub async fn renew(&self) -> std::result::Result<(), HostError> {
        let current = lock(&self.held).clone();
        let Some(lease) = current else {
            return Err(HostError::SiteRunning);
        };
        let renewed = self
            .coordinator
            .renew(&lease, self.ttl)
            .await
            .map_err(|_| HostError::SiteRunning)?;
        *lock(&self.held) = Some(renewed);
        Ok(())
    }

    /// Gives the maintenance lease up.
    pub async fn release(&self) {
        let current = lock(&self.held).take();
        if let Some(lease) = current {
            let _ = self.coordinator.release(&lease).await;
        }
    }
}

impl SiteGuard for LeaseGuard {
    fn ensure_stopped<'a>(&'a self, site: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            let shipper = format!("journal-ship:{site}");
            match self.coordinator.current(&shipper).await {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => return Err(HostError::SiteRunning),
            }
            let maintenance = format!("journal-rebuild:{site}");
            let lease = self
                .coordinator
                .acquire(&maintenance, &self.holder, self.ttl)
                .await
                .map_err(|_| HostError::SiteRunning)?;
            *lock(&self.held) = Some(lease);
            Ok(())
        })
    }
}

/// The journal as the rebuild host sees it: the verified manifests and snapshots of one tenant,
/// and the fork written through the relay.
pub struct StoreJournal {
    site: String,
    target: PathBuf,
    forbidden: Vec<PathBuf>,
    manifest_keys: VerifyingKeys,
    snapshot_keys: Arc<dyn SnapshotKeys>,
    keys: Arc<JournalKeys>,
    clock: Arc<dyn Clock>,
}

impl StoreJournal {
    /// The journal of `site` in the folder `target` (outside the `forbidden` directories).
    #[must_use]
    pub fn new(
        site: String,
        target: PathBuf,
        forbidden: Vec<PathBuf>,
        keys: Arc<JournalKeys>,
        snapshot_keys: Arc<dyn SnapshotKeys>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            manifest_keys: keys.verifying(),
            site,
            target,
            forbidden,
            snapshot_keys,
            keys,
            clock,
        }
    }
}

impl JournalSide for StoreJournal {
    fn tenant(&self) -> &str {
        &self.site
    }

    fn catalog(&self) -> Fut<'_, JournalCatalog> {
        Box::pin(async move {
            let reader = FolderReader::new(self.target.clone(), self.forbidden.clone());
            let (segments, forks) = read_catalogue(&reader, &self.manifest_keys, &self.site)
                .await
                .map_err(|code| match code {
                    "journal_wiring.manifest_mismatch" => HostError::TenantMismatch,
                    _ => HostError::JournalFailed,
                })?;
            let snapshots = list_snapshots(&reader, &self.site, &*self.snapshot_keys)
                .await
                .map_err(|_| HostError::JournalFailed)?;
            Ok(JournalCatalog {
                snapshots,
                segments,
                forks,
            })
        })
    }

    fn resolve_before(&self, _millis: i64) -> Fut<'_, Option<(u64, u64)>> {
        // The journal has no time index the planner can read: `before <timestamp>` is refused
        // (`journal_rebuild_host.target_unresolved`) and the operator names a position.
        Box::pin(async { Ok(None) })
    }

    fn record_fork(&self, fork: ForkPoint, new_generation: u64) -> Fut<'_, ()> {
        Box::pin(async move {
            let canonical = check_folder_target(&self.target, &self.forbidden)
                .map_err(|_| HostError::JournalFailed)?;
            let folder = FolderStore::new(canonical).map_err(|_| HostError::JournalFailed)?;
            let store =
                TenantScoped::new(folder, &self.site).map_err(|_| HostError::TenantMismatch)?;
            let config = RelayConfig::new(&self.site, &self.site, Durability::Local)
                .with_tail_keys(self.keys.tail_keys());
            let now = millis(self.clock.as_ref());
            let record = Fork::new(
                new_generation,
                fork.from_position,
                "point_in_time_rebuild",
                now,
            );
            // Opening the generation writes the new manifest with the fork recorded; the relay
            // itself is not kept.
            Relay::open_fork(
                store,
                self.keys.signer(),
                self.manifest_keys.clone(),
                config,
                record,
                now,
            )
            .await
            .map(|_| ())
            .map_err(|_| HostError::JournalFailed)
        })
    }
}

/// The protect layer's views, for revealing protected fields while a rebuild applies them.
#[derive(Clone)]
pub struct Reveal {
    /// The declared personal fields.
    pub fields: SharedFields,
    /// The subject keys.
    pub keys: SharedKeys,
}

/// A borrowed sink as a sink, so the reveal wrapper can own it for one run.
struct ByRef<'s>(&'s mut dyn ApplySink);

impl ApplySink for ByRef<'_> {
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a> {
        self.0.begin(summary)
    }
    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        self.0.apply_image(image)
    }
    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        self.0.apply_delete(delete)
    }
    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_> {
        self.0.apply_audit(audit)
    }
    fn commit(
        &mut self,
        last_position: spot_events_contract::envelope::Position,
    ) -> SinkFuture<'_> {
        self.0.commit(last_position)
    }
    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a> {
        self.0.abort(reason)
    }
}

/// The executor over the read-only journal store: verifies, restores the base snapshot, reads the
/// segments, upcasts, and (with a [`Reveal`]) turns protected fields back into their values.
pub struct StoreRebuilder {
    store: Arc<dyn JournalStore>,
    manifest_keys: VerifyingKeys,
    snapshot_keys: Arc<dyn SnapshotKeys>,
    tenant: TenantId,
    source: String,
    target_schema: u64,
    reveal: Option<Reveal>,
}

impl StoreRebuilder {
    /// An executor for `tenant`/`source`, upcasting to `target_schema`.
    #[must_use]
    pub fn new(
        store: Arc<dyn JournalStore>,
        manifest_keys: VerifyingKeys,
        snapshot_keys: Arc<dyn SnapshotKeys>,
        tenant: TenantId,
        source: String,
        target_schema: u64,
        reveal: Option<Reveal>,
    ) -> Self {
        Self {
            store,
            manifest_keys,
            snapshot_keys,
            tenant,
            source,
            target_schema,
            reveal,
        }
    }
}

impl Rebuilder for StoreRebuilder {
    fn execute<'a>(&'a self, plan: &'a RebuildPlan, sink: &'a mut dyn ApplySink) -> RebuildFut<'a> {
        Box::pin(async move {
            // No upcaster is registered yet: an entity at another schema refuses the rebuild.
            let registry = spot_image_upcast::Registry::new();
            let options =
                RebuildOptions::new(self.tenant.clone(), self.source.clone(), self.target_schema);
            let keys = RebuildKeys {
                manifest: &self.manifest_keys,
                snapshot: &*self.snapshot_keys,
            };
            match &self.reveal {
                Some(reveal) => {
                    let mut wrapped =
                        RevealSink::new(ByRef(sink), reveal.fields.clone(), reveal.keys.clone());
                    spot_journal_rebuild::execute(
                        &*self.store,
                        &keys,
                        &registry,
                        plan,
                        &mut wrapped,
                        &options,
                    )
                    .await
                }
                None => {
                    spot_journal_rebuild::execute(
                        &*self.store,
                        &keys,
                        &registry,
                        plan,
                        sink,
                        &options,
                    )
                    .await
                }
            }
        })
    }
}
