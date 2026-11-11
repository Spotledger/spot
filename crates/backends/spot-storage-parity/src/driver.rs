//! The seam between the runner and a backend: a [`Driver`] runs the few operations a case
//! needs, a [`BackendFactory`] makes a fresh prepared one per case.
//!
//! The runner only sees the driver, so a test can wrap one (the runner tests use a driver that
//! lowercases text) and prove the matrix catches it.

use std::any::Any;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, Change, Document, DocumentData, DocumentStore, ErrorKind, FeedPosition, Isolation,
    Name, Query, QueryExecutor, ReadSource, RecordId, Rows, StorageBackend, StorageError,
    StorageResult, Transaction, TxOptions, Version,
};
use spot_storage_fake::FakeBackend;

/// What a case asks of the backend it runs on before `prepare`: the extensions its scratch
/// database must have (ADR-087 section 4.3, the closed list in [`crate::cases::EXTENSIONS`]).
/// `prepare` never creates an extension itself (SEM-GEO-04), so the harness stands in for the
/// audited `extension.install` operation (WP-STO-12.T2) — and a case that asserts what happens
/// **without** one asks for none.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaseNeeds<'a> {
    /// The extensions to create in the scratch database first.
    pub extensions: &'a [String],
}

/// The operations a case needs. Every write runs in its own transaction and commits, so a
/// failure that PostgreSQL only reports at commit (a deferred link check) shows up as the
/// result of the write.
pub trait Driver: Send + Sync {
    /// Inserts the documents in one transaction. `max_writes` overrides the transaction's
    /// write limit.
    fn insert(
        &self,
        docs: &[DocumentData],
        max_writes: Option<u32>,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Inserts the documents and deletes them again in **one** transaction: the case that must
    /// leave no change-feed entry (SEM-HLD-03).
    fn insert_then_delete(
        &self,
        docs: &[DocumentData],
        max_writes: Option<u32>,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Reads the change feed from `after`.
    fn changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> impl Future<Output = StorageResult<Vec<Change>>> + Send;

    /// Replaces a document, expecting `expected` to be its version.
    fn update(
        &self,
        doc: &DocumentData,
        expected: Version,
        max_writes: Option<u32>,
    ) -> impl Future<Output = StorageResult<Version>> + Send;

    /// Deletes a document, expecting `expected` to be its version.
    fn delete(
        &self,
        entity: &Name,
        id: RecordId,
        expected: Version,
        max_writes: Option<u32>,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Loads a document.
    fn load(
        &self,
        entity: &Name,
        id: RecordId,
    ) -> impl Future<Output = StorageResult<Option<Document>>> + Send;

    /// Runs a query inside a transaction.
    fn query(&self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send;

    /// Runs a query outside a transaction, on the primary.
    fn read(&self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send;
}

/// Makes a fresh, prepared driver per case.
pub trait BackendFactory: Sync {
    /// The driver this factory makes.
    type Driver: Driver;

    /// The backend's name in reports.
    fn name(&self) -> &'static str;

    /// A new empty store prepared with `catalog`, or `Ok(None)` when the backend is not
    /// available in this environment (the runner reports it as skipped). `needs` carries what
    /// the case asked for before `prepare` (the extensions of ADR-087); a backend without
    /// extensions ignores it.
    fn fresh(
        &self,
        catalog: Arc<Catalog>,
        needs: &CaseNeeds<'_>,
    ) -> impl Future<Output = StorageResult<Option<Self::Driver>>> + Send;
}

/// A [`Driver`] over any [`StorageBackend`].
pub struct StorageDriver<B: StorageBackend> {
    backend: B,
    catalog: Arc<Catalog>,
    // Dropped after the backend: a scratch database must outlive its connections.
    _guard: Option<Box<dyn Any + Send + Sync>>,
}

impl<B: StorageBackend> StorageDriver<B> {
    /// Wraps a prepared backend. `guard` is kept alive as long as the driver.
    pub fn new(
        backend: B,
        catalog: Arc<Catalog>,
        guard: Option<Box<dyn Any + Send + Sync>>,
    ) -> Self {
        Self {
            backend,
            catalog,
            _guard: guard,
        }
    }

    async fn begin(&self, max_writes: Option<u32>) -> StorageResult<B::Tx> {
        let label = Name::new("parity")?;
        let mut options = TxOptions::new(label, Isolation::ReadCommitted);
        if let Some(m) = max_writes {
            options.max_writes = m;
        }
        self.backend.begin(self.catalog.clone(), options).await
    }
}

impl<B: StorageBackend> Driver for StorageDriver<B> {
    async fn insert(&self, docs: &[DocumentData], max_writes: Option<u32>) -> StorageResult<()> {
        let mut tx = self.begin(max_writes).await?;
        for d in docs {
            tx.insert(d).await?;
        }
        tx.commit().await
    }

    async fn insert_then_delete(
        &self,
        docs: &[DocumentData],
        max_writes: Option<u32>,
    ) -> StorageResult<()> {
        let mut tx = self.begin(max_writes).await?;
        for d in docs {
            tx.insert(d).await?;
        }
        for d in docs {
            tx.delete(&d.entity, d.id, Version::FIRST).await?;
        }
        tx.commit().await
    }

    async fn changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        self.backend.read_changes(after, limit).await
    }

    async fn update(
        &self,
        doc: &DocumentData,
        expected: Version,
        max_writes: Option<u32>,
    ) -> StorageResult<Version> {
        let mut tx = self.begin(max_writes).await?;
        let v = tx.update(doc, expected).await?;
        tx.commit().await?;
        Ok(v)
    }

    async fn delete(
        &self,
        entity: &Name,
        id: RecordId,
        expected: Version,
        max_writes: Option<u32>,
    ) -> StorageResult<()> {
        let mut tx = self.begin(max_writes).await?;
        tx.delete(entity, id, expected).await?;
        tx.commit().await
    }

    async fn load(&self, entity: &Name, id: RecordId) -> StorageResult<Option<Document>> {
        let mut tx = self.begin(None).await?;
        let doc = tx
            .load(entity, id, spot_storage_contract::RowLock::None)
            .await?;
        tx.rollback().await?;
        Ok(doc)
    }

    async fn query(&self, query: &Query) -> StorageResult<Rows> {
        let mut tx = self.begin(None).await?;
        let rows = tx.query(query).await?;
        tx.rollback().await?;
        Ok(rows)
    }

    async fn read(&self, query: &Query) -> StorageResult<Rows> {
        self.backend
            .read(self.catalog.clone(), ReadSource::Primary, query)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_server_skips_unless_required() {
        assert_eq!(pg_decision(true, false), PgDecision::Run);
        assert_eq!(pg_decision(true, true), PgDecision::Run);
        assert_eq!(pg_decision(false, false), PgDecision::Skip);
        assert_eq!(pg_decision(false, true), PgDecision::Fail);
    }
}

/// The in-memory fake.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeFactory;

impl BackendFactory for FakeFactory {
    type Driver = StorageDriver<FakeBackend>;

    fn name(&self) -> &'static str {
        "fake"
    }

    async fn fresh(
        &self,
        catalog: Arc<Catalog>,
        _needs: &CaseNeeds<'_>,
    ) -> StorageResult<Option<Self::Driver>> {
        let backend = FakeBackend::new();
        backend.prepare(&catalog).await?;
        Ok(Some(StorageDriver::new(backend, catalog, None)))
    }
}

/// PostgreSQL, on one scratch database per case (`SPOT_TEST_PG_URL`). Without a server the
/// factory reports "not available" after printing one notice; with `SPOT_REQUIRE_PG=1` that is
/// a failure instead, so CI cannot pass by skipping.
#[derive(Debug, Default, Clone, Copy)]
pub struct PgFactory;

static NOTICE_PRINTED: AtomicBool = AtomicBool::new(false);

/// Whether `SPOT_REQUIRE_PG=1` is set.
pub fn pg_required() -> bool {
    std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1")
}

/// What to do about PostgreSQL, given whether `SPOT_TEST_PG_URL` is set and whether
/// `SPOT_REQUIRE_PG=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgDecision {
    /// Run on PostgreSQL.
    Run,
    /// Skip PostgreSQL, with a printed notice.
    Skip,
    /// Fail: PostgreSQL is required and not configured.
    Fail,
}

/// The decision table behind [`PgFactory`]: a missing server is a skip, unless it is required.
pub fn pg_decision(url_set: bool, required: bool) -> PgDecision {
    match (url_set, required) {
        (true, _) => PgDecision::Run,
        (false, false) => PgDecision::Skip,
        (false, true) => PgDecision::Fail,
    }
}

impl BackendFactory for PgFactory {
    type Driver = StorageDriver<PgBackend>;

    fn name(&self) -> &'static str {
        "postgres"
    }

    async fn fresh(
        &self,
        catalog: Arc<Catalog>,
        needs: &CaseNeeds<'_>,
    ) -> StorageResult<Option<Self::Driver>> {
        match pg_decision(std::env::var("SPOT_TEST_PG_URL").is_ok(), pg_required()) {
            PgDecision::Run => {}
            PgDecision::Fail => {
                return Err(StorageError::new(
                    ErrorKind::Unavailable,
                    "SPOT_REQUIRE_PG=1 but SPOT_TEST_PG_URL is not set",
                ));
            }
            PgDecision::Skip => {
                if !NOTICE_PRINTED.swap(true, Ordering::Relaxed) {
                    println!(
                        "SKIPPED: PostgreSQL parity not run; set SPOT_TEST_PG_URL (or SPOT_REQUIRE_PG=1 to fail)"
                    );
                }
                return Ok(None);
            }
        }
        let Some(db) = TestDb::create().await else {
            return Ok(None);
        };
        // ADR-087 section 4.3: an extension is created only through the audited
        // `extension.install` operation (WP-STO-12.T2). The harness stands in for it here, so a
        // case that needs PostGIS gets it and a case that asserts its absence asks for nothing.
        // A server that does not offer one is a failure naming the package: never a skip.
        let names: Vec<&str> = needs.extensions.iter().map(String::as_str).collect();
        let missing = db.create_extensions(&names).await;
        if let Some(ext) = missing.first() {
            return Err(StorageError::new(
                ErrorKind::Unavailable,
                format!(
                    "the PostgreSQL server does not offer the extension `{ext}`: install the \
                     package postgresql-18-postgis-3 (or postgresql-contrib-18) on the runner \
                     (ADR-087; the parity suite's construct cases need it)"
                ),
            ));
        }
        let backend = PgBackend::connect(db.config()).await?;
        backend.prepare(&catalog).await?;
        Ok(Some(StorageDriver::new(
            backend,
            catalog,
            Some(Box::new(db)),
        )))
    }
}
