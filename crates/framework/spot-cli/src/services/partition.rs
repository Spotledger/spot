//! The site's ownership registry: `spot-partition-pg` behind a bounded, refreshed snapshot
//! (WP-PRD-01.T21).
//!
//! # Why a snapshot sits in front of the store
//!
//! [`spot_partition::PartitionRegistry`] is a synchronous trait, so `spot-partition-pg` reaches
//! PostgreSQL through the one shared blocking bridge. ADR-082's addendum says a bridge store is
//! built and used at install and start-up, never from a request handler: a bridge call inside an
//! async task is refused with `pg_bridge.wrong_thread`, and a call inside a transaction an async
//! store holds could deadlock the pool.
//!
//! The owner check **is** on the write path: `OwnerParticipant::before_write` calls
//! [`spot_partition::check_write`], which resolves `map_at(instant)` for every write. Handing the
//! participant a [`PgPartitionRegistry`] directly would therefore put a bridge call inside the
//! request task on every write — refused, and so every write would fail closed. This module
//! supplies the sanctioned shape instead:
//!
//! - a **plain thread** reads the registry's audit trail (every assignment with its effective
//!   interval) at start-up and then every [`REFRESH_INTERVAL`], and swaps an immutable snapshot
//!   in;
//! - [`SnapshotRegistry`] answers `map_at` from that snapshot in memory, resolving the requested
//!   instant against the intervals it holds, so a write costs a lock and a scan of the
//!   assignments, never a database round trip and never a bridge call;
//! - `assign` and `history` go to the store as they are: they belong to the control plane, and a
//!   caller that reaches them from a request task is refused by the bridge itself rather than
//!   being served a snapshot.
//!
//! The snapshot is bounded and fail-closed, by two exact rules:
//!
//! - **Maximum staleness.** A refresh that fails keeps the last good snapshot and counts the
//!   failure ([`SnapshotRegistry::failures`]), but only for [`MAX_STALENESS`]: once the last
//!   successful refresh is older than that (by the registry's clock), `map_at` answers an empty
//!   map, which refuses every write rather than allowing one under ownership that may have moved
//!   ([`SnapshotRegistry::is_stale`]). The next successful refresh restores the answer. A read
//!   that never succeeded also answers an empty map.
//! - **Truncation keeps active assignments.** A trail longer than [`MAX_CHANGES`] drops closed
//!   history first, never a live assignment (see [`MAX_CHANGES`]).
//!
//! A change of ownership reaches a running site without a restart, within one refresh interval.
//!
//! # One bridge per host, one registry per site
//!
//! There is one bridge per database (`spot-pg-bridge`'s "one per process": a second bridge is a
//! second runtime and a second set of limits), built here on a plain thread and shared by every
//! site that answers on that database. Each site gets its own registry over its own pool, because
//! the registry's tables live in the site's database.
//!
//! The module reads the time only through the clock it is given ([`SnapshotRegistry::start_with_clock`];
//! [`SnapshotRegistry::start`] passes the system clock): the owner check's instant stays the
//! write's own business time, and the clock only dates refreshes and judges staleness.
//!
//! # What an operator sees
//!
//! Readiness reports `partition: node <id>, <n> DocTypes` (see
//! [`crate::services::participants::ParticipantsReady`]) and never an assignment. A registry the
//! site cannot build or prepare refuses the start-up with a stable code: an ownership check that
//! cannot read ownership must stop the site, not warn.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use spot_partition::{
    Assignment, PartitionChange, PartitionError, PartitionMap, PartitionRegistry,
};
use spot_partition_pg::PgPartitionRegistry;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use time::OffsetDateTime;

use crate::services::Level;
use crate::{Error, Result};

/// How often the snapshot is refreshed: one small read of the assignment trail. A failed refresh
/// is retried at the next tick; the snapshot it leaves is served for at most [`MAX_STALENESS`].
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// How old the last successful refresh may be before `map_at` fails closed: ten refresh
/// intervals, so a few transient read failures never refuse writes, while a node that lost its
/// database for minutes stops writing under ownership that may have moved.
pub const MAX_STALENESS: Duration = Duration::from_secs(300);

/// The most ownership changes a snapshot keeps before it drops closed history.
///
/// Above this, changes are dropped oldest first: first superseded versions of a scope key, then
/// closed assignments (an `effective_to` that is not in the future). A change that is the current
/// (highest-version) assignment of its scope key and is open-ended or ends in the future is
/// **never dropped**; if those alone exceed the limit they are all kept (and
/// `partition.snapshot_large` is logged). An instant older than the kept history resolves to no
/// owner, which refuses the write rather than allowing one this node may not own.
pub const MAX_CHANGES: usize = 4096;

/// The trail the snapshot is built from: the store, or a fake in tests.
pub trait TrailSource: Send + Sync {
    /// Every ownership change, oldest first.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the read fails.
    fn try_history(&self) -> std::result::Result<Vec<PartitionChange>, PartitionError>;

    /// Records an assignment; returns its version.
    ///
    /// # Errors
    ///
    /// The store's own error.
    fn assign(
        &self,
        request: spot_partition::AssignmentRequest,
    ) -> std::result::Result<u64, PartitionError>;

    /// The audit trail, as the store answers it.
    fn history(&self) -> Vec<PartitionChange>;
}

impl TrailSource for PgPartitionRegistry {
    fn try_history(&self) -> std::result::Result<Vec<PartitionChange>, PartitionError> {
        PgPartitionRegistry::try_history(self)
    }

    fn assign(
        &self,
        request: spot_partition::AssignmentRequest,
    ) -> std::result::Result<u64, PartitionError> {
        PartitionRegistry::assign(self, request)
    }

    fn history(&self) -> Vec<PartitionChange> {
        PartitionRegistry::history(self)
    }
}

/// The instant source of a registry.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// The changes a snapshot keeps, per the [`MAX_CHANGES`] rule, in their original order, and
/// whether the active assignments alone exceed the limit.
fn retain(changes: Vec<PartitionChange>, now: OffsetDateTime) -> (Vec<PartitionChange>, bool) {
    if changes.len() <= MAX_CHANGES {
        return (changes, false);
    }
    let mut newest: BTreeMap<(String, String), u64> = BTreeMap::new();
    for change in &changes {
        let key = (
            change.scope.key_type().to_owned(),
            change.scope.value().to_owned(),
        );
        let slot = newest.entry(key).or_insert(change.version);
        if *slot < change.version {
            *slot = change.version;
        }
    }
    // 0 = active (kept), 1 = superseded, 2 = closed current.
    let class = |change: &PartitionChange| {
        let key = (
            change.scope.key_type().to_owned(),
            change.scope.value().to_owned(),
        );
        if newest.get(&key).is_some_and(|v| *v > change.version) {
            1_u8
        } else if change.effective_to.is_none_or(|to| to > now) {
            0
        } else {
            2
        }
    };
    let classes: Vec<u8> = changes.iter().map(class).collect();
    let active = classes.iter().filter(|c| **c == 0).count();
    let mut keep = vec![true; changes.len()];
    let mut excess = changes.len().saturating_sub(MAX_CHANGES);
    for drop_class in [1_u8, 2] {
        for (index, c) in classes.iter().enumerate() {
            if excess == 0 {
                break;
            }
            if *c == drop_class {
                keep[index] = false;
                excess -= 1;
            }
        }
    }
    let kept = changes
        .into_iter()
        .zip(keep)
        .filter_map(|(change, keep)| keep.then_some(change))
        .collect();
    (kept, active > MAX_CHANGES)
}

/// The tables the registry creates for itself.
pub const TABLES: [&str; 4] = spot_partition_pg::schema::TABLES;

/// Runs `work` on a plain thread and waits for it.
///
/// Every call that crosses the bridge from start-up or from a CLI command runs here: the bridge
/// refuses a call whose thread belongs to a tokio runtime (`pg_bridge.wrong_thread`), and
/// `spot-partition-pg` refuses to be built there (`partition_pg.unsupported_context`).
///
/// # Errors
///
/// The work's own error, or one saying the thread failed.
pub fn on_plain_thread<T, F>(what: &str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    std::thread::spawn(work)
        .join()
        .map_err(|_| Error(format!("the {what} thread failed")))?
}

/// The parts one snapshot is made of: the store reads from, the current snapshot, and the
/// counters an operator can read.
struct Shared {
    store: Arc<dyn TrailSource>,
    clock: Clock,
    snapshot: RwLock<Arc<Vec<PartitionChange>>>,
    failures: AtomicU64,
    refreshed_at: Mutex<Option<OffsetDateTime>>,
    stale: AtomicBool,
    stale_transitions: AtomicU64,
}

impl Shared {
    /// Reads the trail and swaps it in; keeps the last good snapshot when the read fails.
    fn refresh(&self, at: OffsetDateTime) -> std::result::Result<(), PartitionError> {
        match self.store.try_history() {
            Ok(changes) => {
                let (changes, large) = retain(changes, at);
                if large {
                    tracing::warn!(
                        count = changes.len(),
                        "partition.snapshot_large: active assignments alone exceed MAX_CHANGES; all kept"
                    );
                }
                let mut slot = self.snapshot.write().unwrap_or_else(|p| p.into_inner());
                *slot = Arc::new(changes);
                *self.refreshed_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(at);
                if self.stale.swap(false, Ordering::AcqRel) {
                    tracing::info!(
                        "partition.snapshot_fresh: the ownership snapshot is fresh again"
                    );
                }
                Ok(())
            }
            Err(error) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    /// Whether the last successful refresh is older than [`MAX_STALENESS`] (or none happened).
    /// Logs `partition.snapshot_stale` once per transition into staleness, not per call.
    fn is_stale(&self) -> bool {
        let now = (self.clock)();
        let refreshed = *self.refreshed_at.lock().unwrap_or_else(|p| p.into_inner());
        let stale = refreshed.is_none_or(|at| now - at > MAX_STALENESS);
        if stale && !self.stale.swap(true, Ordering::AcqRel) {
            self.stale_transitions.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "partition.snapshot_stale: the ownership snapshot is too old; every write is refused until a refresh succeeds"
            );
        }
        stale
    }

    fn map_at(&self, at: OffsetDateTime) -> PartitionMap {
        if self.is_stale() {
            return PartitionMap::new(Vec::new());
        }
        let changes = Arc::clone(&self.snapshot.read().unwrap_or_else(|p| p.into_inner()));
        resolve(&changes, at)
    }
}

/// The ownership map effective at `at`: every assignment whose interval covers the instant, the
/// highest version per scope key — the same rule [`spot_partition::MemoryPartitionRegistry`] and
/// `spot-partition-pg` apply.
fn resolve(changes: &[PartitionChange], at: OffsetDateTime) -> PartitionMap {
    let mut by_scope: BTreeMap<(String, String), &PartitionChange> = BTreeMap::new();
    for change in changes {
        if change.effective_from > at {
            continue;
        }
        if change.effective_to.is_some_and(|to| at >= to) {
            continue;
        }
        let key = (
            change.scope.key_type().to_owned(),
            change.scope.value().to_owned(),
        );
        let newer = by_scope
            .get(&key)
            .is_none_or(|current| current.version < change.version);
        if newer {
            by_scope.insert(key, change);
        }
    }
    PartitionMap::new(
        by_scope
            .into_values()
            .map(|change| Assignment {
                scope: change.scope.clone(),
                node: change.node.clone(),
                version: change.version,
                effective_from: change.effective_from,
                effective_to: change.effective_to,
            })
            .collect(),
    )
}

/// The snapshot's readiness answer (see [`SnapshotRegistry::health`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHealth {
    /// Always [`Level::Optional`]: a stale snapshot degrades the site, it does not stop reads.
    pub level: Level,
    /// True while the snapshot is stale and writes to partitioned scopes are refused.
    pub degraded: bool,
    /// The operator-facing line: age in whole seconds and the failure count, nothing else.
    pub detail: String,
}

/// The site's ownership registry: the store's answer, from a snapshot refreshed off the request
/// path.
pub struct SnapshotRegistry {
    shared: Arc<Shared>,
    stop: std::sync::mpsc::Sender<()>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for SnapshotRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotRegistry")
            .field("refreshed_at", &self.refreshed_at())
            .field("failures", &self.failures())
            .finish_non_exhaustive()
    }
}

impl SnapshotRegistry {
    /// Reads the trail once on the calling thread and then keeps it fresh on a thread of its own.
    ///
    /// Call this off an async runtime (see [`on_plain_thread`]); the first read crosses the
    /// bridge. The thread stops when the registry is dropped.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the first read fails: a site whose ownership cannot be read
    /// at start-up does not serve.
    pub fn start(
        store: Arc<PgPartitionRegistry>,
        at: OffsetDateTime,
        interval: Duration,
    ) -> std::result::Result<Arc<Self>, PartitionError> {
        Self::start_with_clock(store, at, interval, Arc::new(OffsetDateTime::now_utc))
    }

    /// [`start`](Self::start) over any trail source and an explicit clock. The clock dates every
    /// refresh after the first (which is dated `at`) and judges [`MAX_STALENESS`]; tests pass a
    /// manual one.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the first read fails.
    pub fn start_with_clock(
        store: Arc<dyn TrailSource>,
        at: OffsetDateTime,
        interval: Duration,
        clock: Clock,
    ) -> std::result::Result<Arc<Self>, PartitionError> {
        let shared = Arc::new(Shared {
            store,
            clock,
            snapshot: RwLock::new(Arc::new(Vec::new())),
            failures: AtomicU64::new(0),
            refreshed_at: Mutex::new(None),
            stale: AtomicBool::new(false),
            stale_transitions: AtomicU64::new(0),
        });
        shared.refresh(at)?;
        let (stop, stopped) = std::sync::mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("spot-partition-snapshot".to_owned())
                .spawn(move || {
                    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        stopped.recv_timeout(interval)
                    {
                        // A failed read keeps what the last good one saw, for MAX_STALENESS.
                        let _ = shared.refresh((shared.clock)());
                    }
                })
                .map_err(|_| PartitionError::Store {
                    reason: "the snapshot thread could not be started".to_owned(),
                })?
        };
        Ok(Arc::new(Self {
            shared,
            stop,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Reads the trail now and swaps it in, on the calling thread.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the read fails; the last good snapshot stays in use.
    pub fn refresh(&self) -> std::result::Result<(), PartitionError> {
        self.shared.refresh((self.shared.clock)())
    }

    /// How many refreshes failed since the registry started.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.shared.failures.load(Ordering::Relaxed)
    }

    /// The instant of the last successful refresh, if one ever happened.
    #[must_use]
    pub fn refreshed_at(&self) -> Option<OffsetDateTime> {
        *self
            .shared
            .refreshed_at
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Whether the snapshot is older than [`MAX_STALENESS`] and so refuses every write.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.shared.is_stale()
    }

    /// The snapshot's readiness answer.
    ///
    /// Stale: [`Level::Optional`] (the site still serves reads) with a detail saying writes to
    /// partitioned scopes are refused. Fresh: `degraded == false`.
    /// The detail carries only the age in whole seconds and the failure count: never a scope key,
    /// node id, user, query text or store error. Nothing is latched: the next call after a good
    /// refresh is fresh again. Time comes only from the registry's clock.
    ///
    /// The host's readiness composition must call this and surface `detail` (and `degraded`).
    #[must_use]
    pub fn health(&self) -> SnapshotHealth {
        let stale = self.is_stale();
        let age = self
            .refreshed_at()
            .map_or(0, |at| ((self.shared.clock)() - at).whole_seconds().max(0));
        let failures = self.failures();
        let detail = if stale {
            format!(
                "ownership snapshot stale: writes to partitioned scopes are refused (age {age}s, failures {failures})"
            )
        } else {
            format!("ownership snapshot fresh (age {age}s, failures {failures})")
        };
        SnapshotHealth {
            level: Level::Optional,
            degraded: stale,
            detail,
        }
    }

    /// How many times the snapshot went from fresh to stale (one `partition.snapshot_stale` log
    /// line each).
    #[must_use]
    pub fn stale_transitions(&self) -> u64 {
        self.shared.stale_transitions.load(Ordering::Relaxed)
    }

    /// How many ownership changes the snapshot holds.
    #[must_use]
    pub fn changes(&self) -> usize {
        self.shared
            .snapshot
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

impl Drop for SnapshotRegistry {
    fn drop(&mut self) {
        // The thread ends at its next timeout or at once when the stop message arrives.
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

impl PartitionRegistry for SnapshotRegistry {
    /// Records an assignment **in the store**, then refreshes the snapshot.
    ///
    /// An assignment is a control-plane write (a hub's or an administrator's), never a request's:
    /// called from a request task, the bridge refuses it with `pg_bridge.wrong_thread`.
    fn assign(
        &self,
        request: spot_partition::AssignmentRequest,
    ) -> std::result::Result<u64, PartitionError> {
        let version = self.shared.store.assign(request)?;
        // The caller asked for this change; do not wait for the next tick to honour it. A failed
        // refresh is not the assignment's failure — the change is committed — so it only counts.
        let _ = self.refresh();
        Ok(version)
    }

    /// In memory, from the snapshot: the write path's only read.
    fn map_at(&self, at: OffsetDateTime) -> PartitionMap {
        self.shared.map_at(at)
    }

    /// The audit trail, from the store: an operator's read, not a request's.
    fn history(&self) -> Vec<PartitionChange> {
        self.shared.store.history()
    }
}

/// The readiness check `partition:<site>`: renders [`SnapshotRegistry::health`].
///
/// Always `ok` (a stale snapshot still serves reads, so the node stays in rotation); a stale one
/// starts its detail with `degraded:`, the convention of `LiveReady` and `ServiceHealth`. It reads
/// in-memory age and counters only: no I/O, no wait on the store, and nothing is latched.
pub struct SnapshotReady {
    name: String,
    registry: Arc<SnapshotRegistry>,
}

impl SnapshotReady {
    /// The check of `site`'s registry.
    #[must_use]
    pub fn new(site: &str, registry: Arc<SnapshotRegistry>) -> Self {
        Self {
            name: format!("partition:{site}"),
            registry,
        }
    }
}

impl spot_http::health::ReadinessCheck for SnapshotReady {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_http::health::CheckResult> + Send + '_>>
    {
        let health = self.registry.health();
        Box::pin(async move {
            let mut result = spot_http::health::CheckResult::ok();
            result.detail = if health.degraded {
                format!("degraded: {}", health.detail)
            } else {
                health.detail
            };
            result
        })
    }
}

/// The host's partition wiring: one bridge per database, one registry per site.
#[derive(Default)]
pub struct Host {
    bridges: Mutex<BTreeMap<String, Arc<Bridge>>>,
    sites: Mutex<BTreeMap<String, Arc<SnapshotRegistry>>>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("partition::Host")
            .field(
                "bridges",
                &self.bridges.lock().map(|b| b.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl Host {
    /// A host with no bridge and no registry yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The ownership registry of `site`, or `None` for a single-node site (no `[partition]`).
    ///
    /// The bridge and the registry are built on the first call for a site and reused afterwards,
    /// so a metadata reload does not open a second bridge and does not start a second refresh
    /// thread.
    ///
    /// # Errors
    ///
    /// `partition_pg.unsupported_context` when the bridge's `max_in_flight` cannot fit the site's
    /// pool, and `partition_pg.store` when ownership cannot be read at all.
    pub fn for_site(
        &self,
        site: &crate::config::Site,
        backend: &PgBackend,
        pool_size: usize,
    ) -> Result<Option<Arc<SnapshotRegistry>>> {
        if site.partition.is_none() {
            return Ok(None);
        }
        if let Some(registry) = self
            .sites
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site.site)
        {
            return Ok(Some(Arc::clone(registry)));
        }
        let bridge = self.bridge(&site.database, backend, pool_size)?;
        let registry = {
            let bridge = Arc::clone(&bridge);
            let site_name = site.site.clone();
            on_plain_thread("partition registry", move || {
                let store = PgPartitionRegistry::new(bridge, pool_size)
                    .map_err(|e| Error(format!("{e}")))?;
                SnapshotRegistry::start(
                    Arc::new(store),
                    OffsetDateTime::now_utc(),
                    REFRESH_INTERVAL,
                )
                .map_err(|e| Error(format!("site `{site_name}`: {e}")))
            })?
        };
        self.sites
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(site.site.clone(), Arc::clone(&registry));
        Ok(Some(registry))
    }

    /// The registry `site` serves with now, without building one: `None` for a site that has no
    /// registry (single-node, or one that did not start).
    #[must_use]
    pub fn registry(&self, site: &str) -> Option<Arc<SnapshotRegistry>> {
        self.sites
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(site)
            .map(Arc::clone)
    }

    /// The readiness check of a partitioned site (`partition:<site>`), reading the registry the
    /// site uses now; `None` for a site without `[partition]` or without a registry.
    #[must_use]
    pub fn readiness(&self, site: &crate::config::Site) -> Option<SnapshotReady> {
        site.partition.as_ref()?;
        self.registry(&site.site)
            .map(|registry| SnapshotReady::new(&site.site, registry))
    }

    /// The bridge for `database`, built on the first call.
    ///
    /// `spot-pg-bridge` allows one bridge per process because a bridge is a runtime and a set of
    /// limits; one process serves one database per site, so the bridge is shared by every site on
    /// a database. `max_in_flight` is the registry crate's sizing for a control-plane store (2 to
    /// 4; assignments and grants are rare, short writes) clamped to what the pool can serve.
    fn bridge(&self, database: &str, backend: &PgBackend, pool_size: usize) -> Result<Arc<Bridge>> {
        if let Some(bridge) = self
            .bridges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(database)
        {
            return Ok(Arc::clone(bridge));
        }
        let largest = BridgeConfig::largest_in_flight(pool_size);
        if largest < 1 {
            return Err(Error(format!(
                "cli.partition_registry_missing: `[partition]` needs a PostgreSQL pool of at \
                 least 3 connections (postgres.pool_max_size is {pool_size}), and the ownership \
                 registry cannot be built on this one"
            )));
        }
        let config = BridgeConfig {
            name: "partition".to_owned(),
            max_in_flight: largest.min(4),
            workers: 2,
            queue_capacity: 8,
            // The registry's own deadlines are 10 to 30 seconds; the bridge's ceiling must not
            // cut them short, and its default is the shortest read it makes.
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(30),
        };
        let backend = backend.clone();
        let bridge = Arc::new(on_plain_thread("partition bridge", move || {
            Bridge::for_backend(config, &backend).map_err(|e| Error(format!("{e}")))
        })?);
        self.bridges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(database.to_owned(), Arc::clone(&bridge));
        Ok(bridge)
    }
}

/// Creates the registry's tables for the site's database: the DDL is the owning crate's, run under
/// its own advisory lock, and it is idempotent.
///
/// `spot migrate` calls this, never a request and never a serve start-up: the migrator is the one
/// path that brings a database up to what a site needs.
///
/// # Errors
///
/// `partition_pg.prepare_failed` when the DDL cannot be applied.
pub async fn prepare(backend: &PgBackend, pool_size: usize) -> Result<()> {
    let backend = backend.clone();
    on_plain_thread("partition tables", move || {
        let bridge = Bridge::for_backend(bridge_config(pool_size), &backend)
            .map_err(|e| Error(format!("{e}")))?;
        let store = PgPartitionRegistry::new(Arc::new(bridge), pool_size)
            .map_err(|e| Error(format!("{e}")))?;
        store.prepare().map_err(|e| Error(format!("{e}")))
    })
}

/// The ownership tables the site's database does not hold yet.
///
/// A serve start-up calls this and refuses with "run `spot migrate --apply`", exactly as it does
/// for the participants' other tables.
///
/// # Errors
///
/// [`Error`] when the site's schema cannot be read.
pub async fn missing_tables(backend: &PgBackend) -> Result<Vec<String>> {
    let live = spot_pg_migrate::LiveSchema::read(backend)
        .await
        .map_err(|e| Error(e.to_string()))?;
    Ok(TABLES
        .iter()
        .filter(|table| !live.tables.contains_key(**table))
        .map(|table| (*table).to_owned())
        .collect())
}

/// The bridge settings `spot migrate` uses when it only needs the DDL: the same size the host
/// builds, so the command and the host cannot disagree about what the pool can serve.
fn bridge_config(pool_size: usize) -> BridgeConfig {
    BridgeConfig {
        name: "partition_migrate".to_owned(),
        max_in_flight: BridgeConfig::largest_in_flight(pool_size).clamp(1, 4),
        workers: 2,
        queue_capacity: 4,
        default_deadline: Duration::from_secs(30),
        max_deadline: Duration::from_secs(30),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_partition::ScopeKey;
    use spot_sync_protocol::NodeId;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn change(key: &str, node: &str, version: u64, from: i64, to: Option<i64>) -> PartitionChange {
        PartitionChange {
            scope: ScopeKey::new("branch", key).unwrap(),
            node: NodeId::new(node).unwrap(),
            version,
            effective_from: at(from),
            effective_to: to.map(at),
            actor: "admin".to_owned(),
            reason: "test".to_owned(),
            at: at(from),
        }
    }

    #[test]
    fn the_snapshot_resolves_the_instant_it_is_asked_about() {
        let changes = vec![
            change("W7", "node-a", 1, 1_000, Some(2_000)),
            change("W7", "node-b", 2, 2_000, None),
            change("W9", "node-a", 1, 1_500, None),
        ];
        let early = resolve(&changes, at(1_200));
        assert_eq!(
            early.owner(&ScopeKey::new("branch", "W7").unwrap()),
            Some(&NodeId::new("node-a").unwrap())
        );
        assert!(
            early
                .owner(&ScopeKey::new("branch", "W9").unwrap())
                .is_none(),
            "an assignment that does not apply yet is not in the map"
        );
        let late = resolve(&changes, at(2_500));
        assert_eq!(
            late.owner(&ScopeKey::new("branch", "W7").unwrap()),
            Some(&NodeId::new("node-b").unwrap())
        );
        assert_eq!(
            late.owner(&ScopeKey::new("branch", "W9").unwrap()),
            Some(&NodeId::new("node-a").unwrap())
        );
        // The instant the second assignment takes effect is the new owner's.
        assert_eq!(
            resolve(&changes, at(2_000)).owner(&ScopeKey::new("branch", "W7").unwrap()),
            Some(&NodeId::new("node-b").unwrap())
        );
    }

    #[test]
    fn an_empty_snapshot_owns_nothing() {
        assert!(resolve(&[], at(5_000)).entries().is_empty());
    }
}
