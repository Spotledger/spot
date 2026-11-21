//! The backend: one tenant's store (ADR-34), with introspection and health (ADR-26).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{ErrorKind, StorageError, StorageResult};
use crate::feed::ChangeFeed;
use crate::query::{Query, Rows};
use crate::schema::{Catalog, FieldType};
use crate::tx::{Transaction, TxOptions};

/// Where a read outside a transaction is served from (ADR-47).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSource {
    /// The primary: current data.
    Primary,
    /// A read replica: may lag the primary.
    Replica,
    /// The analytics mirror: may lag further; for BI and heavy analysis.
    AnalyticsMirror,
}

/// What a backend offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Backend name, for example `postgres` or `memory`.
    pub backend: String,
    /// The contract version it implements.
    pub contract_version: String,
    /// Read sources available to [`StorageBackend::read`]; always includes `Primary`.
    pub read_sources: Vec<ReadSource>,
    /// The highest `max_writes` a transaction may declare.
    pub max_writes_per_tx: u32,
    /// Minimum time outbox and change-feed entries are kept.
    pub feed_retention: Duration,
    /// Whether geo filters (`GeoTest`) are served. Geo *values* are always stored; only the
    /// filters need a spatial engine (PostGIS, ADR-087 section 4).
    pub geo_filters: bool,
    /// Whether range values, range filters (`RangeTest`) and exclusions are served (ADR-087
    /// section 5). A backend that sets this to `false` answers `Unsupported` for a catalog
    /// with a range field or an exclusion, and for a query with a range test.
    pub range_exclusion: bool,
    /// Whether search (`Expr::Search`, `Expr::SearchRank`, a search declaration) is served
    /// (ADR-087 section 7).
    pub search: bool,
}

impl Capabilities {
    /// Refuses with `Unsupported` a catalog that needs a construct this backend does not offer:
    /// a range field or an exclusion without `range_exclusion`, a search declaration without
    /// `search`. Backends call it from `prepare`, so the refusal comes from the capability
    /// flags and never from an ad hoc check. Geo fields are never refused here: storing a
    /// point needs no spatial engine on a backend that has no filters.
    pub fn check_catalog(&self, catalog: &Catalog) -> StorageResult<()> {
        for e in catalog.entities() {
            if !self.range_exclusion {
                if let Some(f) = e
                    .fields
                    .iter()
                    .find(|f| matches!(f.ty, FieldType::Range { .. }))
                {
                    return Err(self
                        .unsupported(
                            "range values",
                            &format!("`{}.{}` is a range", e.name, f.name),
                        )
                        .with_entity(&e.name)
                        .with_field(&f.name));
                }
                if let Some(x) = e.exclusions.first() {
                    return Err(self
                        .unsupported("exclusions", &format!("`{}` declares `{}`", e.name, x.name))
                        .with_entity(&e.name)
                        .with_constraint(&x.name));
                }
            }
            if !self.search && e.search.is_some() {
                return Err(self
                    .unsupported(
                        "search",
                        &format!("`{}` declares a search projection", e.name),
                    )
                    .with_entity(&e.name));
            }
        }
        Ok(())
    }

    /// Refuses with `Unsupported` a query that uses a construct this backend does not offer:
    /// `Expr::Geo` without `geo_filters`, `Expr::Range` without `range_exclusion`,
    /// `Expr::Search` or `Expr::SearchRank` without `search`. Backends call it after
    /// [`Query::validate`], so a malformed query is still `InvalidQuery` everywhere.
    pub fn check_query(&self, query: &Query) -> StorageResult<()> {
        let uses = query.constructs();
        if uses.geo && !self.geo_filters {
            return Err(self.unsupported("geo filters", "the query has a geo test"));
        }
        if uses.range && !self.range_exclusion {
            return Err(self.unsupported("range filters", "the query has a range test"));
        }
        if uses.search && !self.search {
            return Err(self.unsupported("search", "the query has a search term"));
        }
        Ok(())
    }

    fn unsupported(&self, what: &str, detail: &str) -> StorageError {
        StorageError::new(
            ErrorKind::Unsupported,
            format!(
                "the `{}` backend does not offer {what} (capabilities advertise it): {detail}",
                self.backend
            ),
        )
    }
}

/// Overall health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthStatus {
    /// Working normally.
    Healthy,
    /// Working with reduced capacity or lag.
    Degraded,
    /// Not serving.
    Down,
}

/// One health probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthCheck {
    /// What was checked, for example `primary_connection` or `replica_lag`.
    pub name: String,
    /// Its result.
    pub status: HealthStatus,
    /// Plain-language detail, including measured values.
    pub detail: String,
}

/// Health of a backend: the worst of its checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthReport {
    /// Overall status.
    pub status: HealthStatus,
    /// Individual checks.
    pub checks: Vec<HealthCheck>,
}

/// One tenant's storage. Cheap to share across tasks (`Send + Sync`); each transaction is
/// owned by one task at a time.
pub trait StorageBackend: ChangeFeed + Send + Sync + 'static {
    /// The transaction type.
    type Tx: Transaction;

    /// What this backend offers.
    fn capabilities(&self) -> Capabilities;

    /// Makes the store able to hold the catalog's entities: creates missing tables, columns
    /// and indexes. Additive only: it never drops or alters existing structures, and a change
    /// it cannot apply additively, including a different collation, fails with `InvalidSchema`.
    /// Full migrations are WP-STO-11.
    fn prepare(&self, catalog: &Catalog) -> impl Future<Output = StorageResult<()>> + Send;

    /// Opens a transaction on the primary against a catalog snapshot.
    fn begin(
        &self,
        catalog: Arc<Catalog>,
        options: TxOptions,
    ) -> impl Future<Output = StorageResult<Self::Tx>> + Send;

    /// Runs a read-only query outside a transaction, from `source`. A query with a lock other
    /// than `RowLock::None` fails with `InvalidQuery`; a source not listed in the capabilities
    /// fails with `Unsupported`.
    fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> impl Future<Output = StorageResult<Rows>> + Send;

    /// Probes the backend.
    fn health(&self) -> impl Future<Output = HealthReport> + Send;
}
