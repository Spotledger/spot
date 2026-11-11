//! Invariants, the registry apps extend, the read-only check context and the report.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_storage_contract::{
    Access, Catalog, Change, FeedPosition, Isolation, Name, OutboxRecord, Query, Rows, Sequences,
    SeriesKey, StorageBackend, StorageError, StorageResult, Transaction, TxOptions,
};
use tokio::sync::Mutex;

use crate::history::{History, OutcomeCounts};
use crate::oracle::Oracle;

/// A boxed future: what an object-safe async method returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The most violations one invariant reports; the rest are only counted.
pub const MAX_VIOLATIONS_PER_INVARIANT: usize = 200;

/// One broken promise. It names records by entity and id and carries a stable, machine-readable
/// `detail` (for example `expected=5 actual=7`); it never carries document content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// The invariant that found it.
    pub invariant: String,
    /// The entity involved (or `outbox`, `sequence`).
    pub entity: String,
    /// The record involved (or the series, or `-`).
    pub id: String,
    /// What is wrong, as `code key=value ...`.
    pub detail: String,
}

impl Violation {
    /// A violation of `invariant`.
    pub fn new(
        invariant: &str,
        entity: impl fmt::Display,
        id: impl fmt::Display,
        detail: impl Into<String>,
    ) -> Violation {
        Violation {
            invariant: invariant.to_owned(),
            entity: entity.to_string(),
            id: id.to_string(),
            detail: detail.into(),
        }
    }

    /// The violation for a check that could not read the store.
    pub fn read_failed(invariant: &str, what: &str, error: &StorageError) -> Violation {
        Violation::new(
            invariant,
            "-",
            "-",
            format!("check_error read={what} code={}", error.kind.code()),
        )
    }
}

/// A property that must hold after every run.
pub trait Invariant: Send + Sync {
    /// A stable id, for example `seq-gapless`. Unique within a registry.
    fn id(&self) -> &str;

    /// One paragraph stating exactly what the invariant proves (and so what it does not).
    fn describe(&self) -> &str;

    /// Checks the invariant and returns every violation found; an empty list means it holds.
    /// A check never stops at the first violation.
    fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>>;
}

/// Read access to a store for checks, independent of the backend type.
pub trait StoreAccess: Send + Sync {
    /// Runs a query inside the check's snapshot (a read-only repeatable-read transaction).
    fn query<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>>;
    /// Reads the whole outbox in position order.
    fn outbox(&self) -> BoxFuture<'_, StorageResult<Vec<OutboxRecord>>>;
    /// Reads the whole change feed in position order.
    fn changes(&self) -> BoxFuture<'_, StorageResult<Vec<Change>>>;
    /// The number the next allocation of `series` would receive, found by allocating in a
    /// transaction that is then rolled back.
    fn next_number<'a>(&'a self, series: &'a str) -> BoxFuture<'a, StorageResult<u64>>;
}

/// [`StoreAccess`] over a storage backend.
pub struct BackendAccess<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
    snapshot: Mutex<Option<B::Tx>>,
}

impl<B: StorageBackend> BackendAccess<B> {
    /// Access to `backend` through `catalog`.
    pub fn new(backend: Arc<B>, catalog: Arc<Catalog>) -> BackendAccess<B> {
        BackendAccess {
            backend,
            catalog,
            snapshot: Mutex::new(None),
        }
    }
}

const FEED_PAGE: u32 = 5_000;

fn check_options(label: &str, isolation: Isolation) -> StorageResult<TxOptions> {
    Ok(TxOptions::new(Name::new(label)?, isolation))
}

impl<B: StorageBackend> StoreAccess for BackendAccess<B> {
    fn query<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async move {
            let mut guard = self.snapshot.lock().await;
            if guard.is_none() {
                let mut options = check_options("l7_check", Isolation::RepeatableRead)?;
                options.access = Access::ReadOnly;
                *guard = Some(self.backend.begin(self.catalog.clone(), options).await?);
            }
            match guard.as_mut() {
                Some(tx) => spot_storage_contract::QueryExecutor::query(tx, query).await,
                None => Err(StorageError::new(
                    spot_storage_contract::ErrorKind::Internal,
                    "no snapshot",
                )),
            }
        })
    }

    fn outbox(&self) -> BoxFuture<'_, StorageResult<Vec<OutboxRecord>>> {
        Box::pin(async move {
            let mut all = Vec::new();
            let mut after = FeedPosition::START;
            loop {
                let page = self.backend.read_outbox(after, FEED_PAGE).await?;
                let Some(last) = page.last() else { break };
                after = last.position;
                let full = page.len() >= FEED_PAGE as usize;
                all.extend(page);
                if !full {
                    break;
                }
            }
            Ok(all)
        })
    }

    fn changes(&self) -> BoxFuture<'_, StorageResult<Vec<Change>>> {
        Box::pin(async move {
            let mut all = Vec::new();
            let mut after = FeedPosition::START;
            loop {
                let page = self.backend.read_changes(after, FEED_PAGE).await?;
                let Some(last) = page.last() else { break };
                after = last.position;
                let full = page.len() >= FEED_PAGE as usize;
                all.extend(page);
                if !full {
                    break;
                }
            }
            Ok(all)
        })
    }

    fn next_number<'a>(&'a self, series: &'a str) -> BoxFuture<'a, StorageResult<u64>> {
        Box::pin(async move {
            let key = SeriesKey::new(series)?;
            let options = check_options("l7_probe", Isolation::ReadCommitted)?;
            let mut tx = self.backend.begin(self.catalog.clone(), options).await?;
            let number = tx.allocate(&key, 1).await;
            // The probe must leave the series as it found it, whatever happened.
            let rolled = tx.rollback().await;
            let number = number?;
            rolled?;
            Ok(number)
        })
    }
}

/// What a check may look at: the store (read-only), the catalogue, the run's history and the
/// oracle built from it.
pub struct CheckContext {
    access: Arc<dyn StoreAccess>,
    catalog: Arc<Catalog>,
    history: Arc<History>,
    oracle: Arc<Oracle>,
}

impl CheckContext {
    /// A context over `access`.
    pub fn new(
        access: Arc<dyn StoreAccess>,
        catalog: Arc<Catalog>,
        history: Arc<History>,
        oracle: Arc<Oracle>,
    ) -> CheckContext {
        CheckContext {
            access,
            catalog,
            history,
            oracle,
        }
    }

    /// A context over a backend.
    pub fn for_backend<B: StorageBackend>(
        backend: Arc<B>,
        catalog: Arc<Catalog>,
        history: Arc<History>,
        oracle: Arc<Oracle>,
    ) -> CheckContext {
        let access = Arc::new(BackendAccess::new(backend, catalog.clone()));
        CheckContext::new(access, catalog, history, oracle)
    }

    /// The catalogue the store holds.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The run's history.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The oracle.
    pub fn oracle(&self) -> &Oracle {
        &self.oracle
    }

    /// Runs a query in the check snapshot.
    pub async fn query(&self, query: &Query) -> StorageResult<Rows> {
        self.access.query(query).await
    }

    /// The whole outbox.
    pub async fn outbox(&self) -> StorageResult<Vec<OutboxRecord>> {
        self.access.outbox().await
    }

    /// The whole change feed.
    pub async fn changes(&self) -> StorageResult<Vec<Change>> {
        self.access.changes().await
    }

    /// The number the next allocation of a series would get.
    pub async fn next_number(&self, series: &str) -> StorageResult<u64> {
        self.access.next_number(series).await
    }
}

/// Why a registration was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// An invariant with this id is already registered.
    Duplicate(String),
    /// The id is empty.
    EmptyId,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Duplicate(id) => write!(f, "invariant `{id}` is already registered"),
            RegistryError::EmptyId => f.write_str("an invariant needs a non-empty id"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The invariants to check after a run. Apps register their own next to the framework's.
#[derive(Default)]
pub struct InvariantRegistry {
    items: Vec<Box<dyn Invariant>>,
}

impl InvariantRegistry {
    /// An empty registry.
    pub fn new() -> InvariantRegistry {
        InvariantRegistry::default()
    }

    /// Adds an invariant; a duplicate or empty id is refused and nothing is added.
    pub fn register(&mut self, invariant: Box<dyn Invariant>) -> Result<(), RegistryError> {
        let id = invariant.id().to_owned();
        if id.is_empty() {
            return Err(RegistryError::EmptyId);
        }
        if self.items.iter().any(|i| i.id() == id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.items.push(invariant);
        Ok(())
    }

    /// The registered ids in registration order.
    pub fn ids(&self) -> Vec<&str> {
        self.items.iter().map(|i| i.id()).collect()
    }

    /// Number of registered invariants.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Runs every invariant, one after another, never stopping at a failure.
    pub async fn check_all(&self, ctx: &CheckContext, meta: RunMeta) -> Report {
        let mut results = Vec::new();
        for invariant in &self.items {
            let mut violations = invariant.check(ctx).await;
            let total = violations.len() as u64;
            violations.truncate(MAX_VIOLATIONS_PER_INVARIANT);
            results.push(InvariantResult {
                id: invariant.id().to_owned(),
                describe: invariant.describe().to_owned(),
                passed: total == 0,
                total_violations: total,
                violations,
            });
        }
        let operations = ctx.history().counts();
        Report {
            schema: REPORT_SCHEMA.to_owned(),
            ok: results.iter().all(|r| r.passed),
            meta,
            operations,
            results,
        }
    }
}

/// The `schema` value of a report.
pub const REPORT_SCHEMA: &str = "spot-l7-report/1";

/// Describes the run a report is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    /// The seed: pass it again to replay the schedule.
    pub seed: u64,
    /// `fake` or `pg`.
    pub backend: String,
    /// Profile name.
    pub profile: String,
    /// Number of workers.
    pub workers: u32,
}

/// The outcome of one invariant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantResult {
    /// The invariant's id.
    pub id: String,
    /// What it proves.
    pub describe: String,
    /// Whether it held.
    pub passed: bool,
    /// Violations found, including those cut from `violations`.
    pub total_violations: u64,
    /// The first violations, at most [`MAX_VIOLATIONS_PER_INVARIANT`].
    pub violations: Vec<Violation>,
}

/// Everything a check found, in JSON or text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Always [`REPORT_SCHEMA`].
    pub schema: String,
    /// True when every invariant held.
    pub ok: bool,
    /// The run.
    pub meta: RunMeta,
    /// Operations by outcome.
    pub operations: OutcomeCounts,
    /// One result per invariant, in registration order.
    pub results: Vec<InvariantResult>,
}

impl Report {
    /// The ids of the invariants that failed.
    pub fn failed_ids(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| r.id.as_str())
            .collect()
    }

    /// The violations of one invariant.
    pub fn violations_of(&self, id: &str) -> &[Violation] {
        self.results
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.violations.as_slice())
            .unwrap_or(&[])
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Plain text for a terminal or a CI log.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let m = &self.meta;
        out.push_str(&format!(
            "L7 invariants: {} on {} (profile {}, {} workers, seed {})\n",
            if self.ok { "OK" } else { "VIOLATIONS" },
            m.backend,
            m.profile,
            m.workers,
            m.seed
        ));
        let o = &self.operations;
        out.push_str(&format!(
            "operations: {} total, {} committed, {} rolled back, {} rejected, {} failed ({} out of retries), {} indeterminate ({} resolved)\n",
            o.total, o.committed, o.rolled_back, o.rejected, o.failed, o.exhausted, o.indeterminate, o.resolved
        ));
        for r in &self.results {
            out.push_str(&format!(
                "  [{}] {}\n",
                if r.passed { "pass" } else { "FAIL" },
                r.id
            ));
            for v in &r.violations {
                out.push_str(&format!("      {} {} {}\n", v.entity, v.id, v.detail));
            }
            if r.total_violations as usize > r.violations.len() {
                out.push_str(&format!(
                    "      ... and {} more\n",
                    r.total_violations as usize - r.violations.len()
                ));
            }
        }
        out
    }
}
