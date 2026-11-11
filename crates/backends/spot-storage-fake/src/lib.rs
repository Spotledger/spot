//! In-memory backend of the storage contract (WP-CON-01.T2).
//!
//! [`FakeBackend`] implements [`StorageBackend`] and passes the storage contract suite, so
//! framework work packages can start before PostgreSQL is ready (blueprint §7, "Fakes unblock
//! early starts"). It is a reference, not a database: everything lives in memory behind one
//! mutex, and nothing is tuned for speed.
//!
//! # Behaviour
//!
//! - **Transactions** keep a private write overlay; reads see committed data merged with it.
//!   `ReadCommitted` reads the latest committed data on every operation. `RepeatableRead` and
//!   `Serializable` read a snapshot taken at `begin`, and fail with `SerializationFailure` when
//!   they write, lock or commit a record another transaction committed after that snapshot.
//!   The fake does **not** detect read-only anomalies (write skew): `Serializable` behaves like
//!   `RepeatableRead`. Finding those is the L8 checker's job against PostgreSQL.
//! - **Locks.** Writes take an exclusive row lock on the document, as PostgreSQL does; loads
//!   and queries lock rows on request; guard locks and sequence series share the same table.
//!   `Wait::Block` waits until the lock is released or the transaction's lock timeout passes.
//!   There is no deadlock detection: a deadlock ends in `LockTimeout`. Row locks are refused
//!   with `ReadOnlyTransaction` in read-only transactions, as on PostgreSQL.
//! - **Constraints.** Unique indexes are checked at write time and again at commit. Database
//!   links (and child rows' `parent`) are checked at commit only, so documents may be written
//!   in any order and link to each other in a cycle.
//! - **Feeds.** Outbox events and change-feed entries are published at commit, under the mutex.
//!   Positions use generation 1; entries are never pruned.
//! - **Loads** return every declared child table of the document, empty ones included, and
//!   exactly the fields declared in the transaction's catalog.
//! - **Not modelled:** statement timeouts, connection failures (`Unavailable`,
//!   `CommitOutcomeUnknown`) and read replicas.
//! - **Geo, range, exclusion and search (ADR-087, option B).** The fake stores and round-trips
//!   geo points exactly, validates them, and accepts geo fields in a catalog. It does not model
//!   geo filters, range values, overlap, exclusion constraints or search: it advertises their
//!   absence in [`Capabilities`], and a catalog or query that needs one fails with
//!   `Unsupported` from those flags (`Capabilities::check_catalog`, `Capabilities::check_query`),
//!   after the shared validation, never by approximating. Those cases run on PostgreSQL.

mod constraints;
mod feed;
mod graph;
mod locks;
mod query;
mod store;
#[cfg(test)]
mod tests;
mod tx;
mod validate;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use spot_storage_contract::{
    CONTRACT_VERSION, Capabilities, Catalog, Change, ChangeFeed, Collation, EntityDef, ErrorKind,
    FeedPosition, HealthCheck, HealthReport, HealthStatus, Isolation, Name, OutboxRecord, Query,
    ReadSource, RecordId, RowLock, Rows, SeriesKey, StorageBackend, StorageError, StorageResult,
    TxOptions, Value,
};
use tokio::sync::Notify;

use crate::feed::Feeds;
use crate::locks::{LockTable, NoRowLocks};
use crate::store::{Data, View};
pub use crate::tx::FakeTx;

/// The highest `max_writes` a transaction may declare.
const MAX_WRITES_PER_TX: u32 = 1_000_000;

/// How long feed entries are kept at least. The fake never prunes.
const FEED_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// An in-memory store for one tenant. Clones share the same store.
#[derive(Clone, Default)]
pub struct FakeBackend {
    shared: Arc<Shared>,
}

impl FakeBackend {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for FakeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeBackend")
    }
}

/// State shared by the backend and its transactions.
#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// Woken whenever a transaction releases its locks.
    pub released: Notify,
}

impl Shared {
    /// Locks the shared state. Never hold the guard across an `.await`.
    pub fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the mutex cannot leave the state half-updated: every
        // mutation is checked first and applied after, so the state stays usable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Everything committed, plus the lock table.
#[derive(Default)]
pub(crate) struct State {
    /// Committed records. Repeatable-read transactions keep a clone of the `Arc` as their
    /// snapshot; commits copy on write.
    pub data: Arc<Data>,
    /// Number of committed transactions that wrote something.
    pub commits: u64,
    /// For each record ever written: the commit number of its last write.
    pub written_at: HashMap<(Name, RecordId), u64>,
    /// Row and guard locks.
    pub locks: LockTable,
    /// Next number of each series that was ever used.
    pub sequences: BTreeMap<SeriesKey, u64>,
    /// Published outbox and change feed.
    pub feeds: Feeds,
    /// What `prepare` has created.
    pub schema: Schema,
}

/// Entities created by `prepare`, and the collation they were created with.
#[derive(Default)]
pub(crate) struct Schema {
    collation: Option<Collation>,
    entities: BTreeMap<Name, EntityDef>,
}

fn schema_err(msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidSchema, msg)
}

impl State {
    /// Applies `catalog` additively: new entities, new nullable fields (or any new field on an
    /// empty entity) and new indexes. Anything else that differs fails with `InvalidSchema`.
    fn prepare(&mut self, catalog: &Catalog) -> StorageResult<()> {
        if let Some(c) = &self.schema.collation
            && c != &catalog.collation
        {
            return Err(schema_err(format!(
                "the store uses collation `{}`; it cannot change to `{}` additively",
                c.as_str(),
                catalog.collation.as_str()
            )));
        }
        let mut added: Vec<(Name, Name)> = Vec::new();
        for e in catalog.entities() {
            let Some(old) = self.schema.entities.get(&e.name) else {
                continue;
            };
            if old.kind != e.kind {
                return Err(schema_err(format!("`{}` cannot change its kind", e.name)));
            }
            for f in &e.fields {
                match old.field(&f.name) {
                    Ok(of) if of.ty != f.ty || of.nullable != f.nullable => {
                        return Err(schema_err(format!(
                            "`{}.{}` cannot change type or nullability additively",
                            e.name, f.name
                        )));
                    }
                    Ok(_) => {}
                    Err(_) if !f.nullable && self.data.has_rows(&e.name) => {
                        return Err(schema_err(format!(
                            "`{}.{}` is required but `{}` already has records",
                            e.name, f.name, e.name
                        )));
                    }
                    Err(_) => added.push((e.name.clone(), f.name.clone())),
                }
            }
            for ix in &e.indexes {
                if let Some(old_ix) = old.indexes.iter().find(|o| o.name == ix.name)
                    && old_ix != ix
                {
                    return Err(schema_err(format!(
                        "index `{}` cannot change its definition",
                        ix.name
                    )));
                }
            }
        }
        if !added.is_empty() {
            let data = Arc::make_mut(&mut self.data);
            for (entity, field) in added {
                for record in data.table_mut(&entity).values_mut() {
                    record.fields.insert(field.clone(), Value::Null);
                }
            }
        }
        self.schema.collation = Some(catalog.collation.clone());
        for e in catalog.entities() {
            let merged = match self.schema.entities.remove(&e.name) {
                // Never drop: keep old fields the new catalog no longer declares.
                Some(mut old) => {
                    for f in &e.fields {
                        if old.field(&f.name).is_err() {
                            old.fields.push(f.clone());
                        }
                    }
                    for ix in &e.indexes {
                        if !old.indexes.iter().any(|o| o.name == ix.name) {
                            old.indexes.push(ix.clone());
                        }
                    }
                    old.children = e.children.clone();
                    old
                }
                None => e.clone(),
            };
            self.schema.entities.insert(e.name.clone(), merged);
        }
        Ok(())
    }
}

/// `SerializationFailure` for a record committed by another transaction after this
/// transaction's snapshot.
pub(crate) fn concurrent_update(entity: &Name) -> StorageError {
    StorageError::new(
        ErrorKind::SerializationFailure,
        format!(
            "a `{entity}` record was changed by another transaction after this transaction's \
             snapshot; retry the whole transaction"
        ),
    )
    .with_entity(entity)
}

/// What the fake offers. It models plain CRUD, relational queries and transactions, and geo
/// storage; it does not model geo filters, range, exclusion or search (ADR-087, option B).
pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        backend: "memory".into(),
        contract_version: CONTRACT_VERSION.into(),
        read_sources: vec![ReadSource::Primary],
        max_writes_per_tx: MAX_WRITES_PER_TX,
        feed_retention: FEED_RETENTION,
        geo_filters: false,
        range_exclusion: false,
        search: false,
    }
}

impl StorageBackend for FakeBackend {
    type Tx = FakeTx;

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        capabilities().check_catalog(catalog)?;
        self.shared.state().prepare(catalog)
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<FakeTx> {
        if options.max_writes > MAX_WRITES_PER_TX {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                format!(
                    "max_writes {} is above this backend's limit of {MAX_WRITES_PER_TX}; chunk \
                     the work",
                    options.max_writes
                ),
            ));
        }
        let snapshot = match options.isolation {
            Isolation::ReadCommitted => None,
            Isolation::RepeatableRead | Isolation::Serializable => {
                let state = self.shared.state();
                Some((state.data.clone(), state.commits))
            }
        };
        Ok(FakeTx::new(self.shared.clone(), catalog, options, snapshot))
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        query.validate(&catalog)?;
        capabilities().check_query(query)?;
        if query.lock != RowLock::None {
            return Err(StorageError::new(
                ErrorKind::InvalidQuery,
                "row locks need a transaction; reads outside one cannot lock",
            ));
        }
        if !self.capabilities().read_sources.contains(&source) {
            return Err(StorageError::new(
                ErrorKind::Unsupported,
                format!("the in-memory backend has no {source:?} read source"),
            ));
        }
        let data = self.shared.state().data.clone();
        query::execute(&catalog, &View::committed(&data), query, &mut NoRowLocks)
    }

    async fn health(&self) -> HealthReport {
        let commits = self.shared.state().commits;
        HealthReport {
            status: HealthStatus::Healthy,
            checks: vec![HealthCheck {
                name: "memory".into(),
                status: HealthStatus::Healthy,
                detail: format!("in-memory store; {commits} committed write transactions"),
            }],
        }
    }
}

impl ChangeFeed for FakeBackend {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        feed::page(&self.shared.state().feeds.outbox, after, limit, "outbox")
    }

    async fn read_changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        feed::page(
            &self.shared.state().feeds.changes,
            after,
            limit,
            "change-feed",
        )
    }
}
