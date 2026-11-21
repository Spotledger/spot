#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]
//! The PostgreSQL + `pgvector` store behind `spot-retrieval` (WP-AI-10.T2, WS13, ADR-070).
//!
//! `spot-retrieval` is the model and the logic: the change-feed indexer, the chunker, the query
//! path that re-checks every hit against the live permission core, and a deterministic in-memory
//! [`VectorStore`]. [`PgVectorStore`] is the durable one. It implements the same three calls
//! over one PostgreSQL table with a `vector(n)` column and an HNSW (or IVFFlat) cosine index, so
//! a production site keeps its index across restarts and ranks inside the database.
//!
//! # The index decides nothing about access
//!
//! The store is a copy of data and holds **no permission data of its own**: no role, owner,
//! share or allowed-user column exists ([`schema`] lists every column, and a test asserts the
//! list). The only rights-shaped column is the names of the fields a chunk's text came from,
//! which `spot-retrieval`'s `Retriever` compares with the caller's live field view. Every hit
//! this store returns is a *candidate*: the `Retriever` asks the permission core, as the calling
//! user and at read time, before a hit leaves that crate, and a test in `tests/` runs a
//! `Retriever` over this store to prove an unreadable document is dropped. The store never
//! returns text it was not given: `body` is written verbatim from the chunk and read back
//! verbatim.
//!
//! # Per tenant, and the tenant in every key
//!
//! A [`PgVectorStore`] is bound to one [`SiteId`] (the tenant, exactly as an indexer serves one
//! site). The tenant is part of the primary key `(tenant, doctype, id, chunk_no)` and every
//! statement filters on it, so a store can neither read nor remove another tenant's rows even
//! on a shared table. The recommended deployment is one retrieval database per site; see
//! [`schema`] for why a shared table can return fewer than `limit` rows for a busy neighbour.
//!
//! # What the store guarantees
//!
//! - **Replace is atomic per call.** `upsert` removes a document's chunks at or below the
//!   incoming version and writes the new ones in one transaction behind a per-document advisory
//!   lock; a reader sees the old chunks or the new ones, never a mixture, and a failure leaves
//!   the old chunks. A stale chunk number (a document that shrank from three chunks to one) is
//!   gone in the same transaction.
//! - **An older version never overwrites a newer one**, so a replayed or out-of-order change is
//!   a no-op, as in the in-memory store.
//! - **Delete is by document**, one transaction for any number of documents
//!   ([`PgVectorStore::delete_documents`]); with the indexer's delete handling this is how a
//!   stale vector for a deleted document leaves the index in the same sweep as the feed entry.
//! - **The width is fixed.** Every vector written or queried must have exactly the width the
//!   store was built for; [`PgVectorStore::prepare`] refuses an existing table of another width.
//! - **Nothing secret in an error.** A failure is `retrieval.store_failed` naming a short cause
//!   and never a chunk, a vector, a tenant, a connection string or the driver's text.
//!
//! # Values are bound, never formatted
//!
//! Every statement is a `const` with `$n` placeholders; the only text that reaches SQL besides a
//! bound value is the table name written in this crate and the integers the index settings
//! chose. A vector crosses the wire as pgvector's text literal and is cast in SQL.
//!
//! # Float arithmetic, and none here
//!
//! The crate does no float arithmetic: the cosine is computed by pgvector in the database and
//! comes back as a `float4` score; the one float comparison is "is every component zero".
//!
//! # Blocking
//!
//! `VectorStore` is synchronous and `tokio-postgres` is not, so each call blocks on the store's
//! runtime. From inside a multi-thread Tokio runtime that is a `block_in_place`;
//! it panics on a current-thread runtime, which is why a host must not call this store there.
//!
//! # Tests and the database
//!
//! The integration tests need PostgreSQL with `pgvector` and skip cleanly (printing `SKIPPED:`)
//! when `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; a server without the
//! extension is also a skip unless `SPOT_REQUIRE_PG=1`, in which case it fails loudly.
//! The [`conformance`] suite runs against the in-memory store with no database at all.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Pool, Runtime as PoolRuntime};
use spot_core::SiteId;
use spot_postgres::{PgConfig, Tls};
use spot_retrieval::{Chunk, RetrievalError, ScoredChunk, VectorStore};
use spot_storage_contract::RecordId;
use tokio_postgres::NoTls;

mod blocking;
pub mod conformance;
mod error;
mod ops;
pub mod schema;
mod wire;

pub use error::PrepareError;
pub use ops::{MAX_DELETE_BATCH, MAX_PAGE, MAX_SEARCH_LIMIT};
pub use schema::{IndexKind, MAX_DIM, MAX_LISTS};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-retrieval-pg";

/// The [`VectorStore`] backed by one PostgreSQL table, bound to one tenant.
pub struct PgVectorStore {
    pool: Pool,
    tenant: SiteId,
    dim: usize,
    index: IndexKind,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl fmt::Debug for PgVectorStore {
    /// Names the tenant, the width and the index method. A pool's configuration can carry a
    /// password, so it is never printed: a `Debug` line ends up in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgVectorStore")
            .field("tenant", &self.tenant)
            .field("dim", &self.dim)
            .field("index", &self.index.method())
            .finish_non_exhaustive()
    }
}

impl PgVectorStore {
    /// A store over `pool`, for `tenant`, with vectors `dim` wide, blocking on `runtime`.
    ///
    /// The pool must reach the database that holds (or will hold, after [`prepare`](Self::prepare))
    /// the retrieval table. This type opens no other connection.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when `dim` is not in `1..=`[`MAX_DIM`] or the index settings
    /// are out of range.
    pub fn new(
        pool: Pool,
        tenant: SiteId,
        dim: usize,
        index: IndexKind,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, RetrievalError> {
        if !(1..=MAX_DIM).contains(&dim) {
            return Err(error::refused(
                "the vector width must be between 1 and 2000 for an indexed column",
            ));
        }
        if !index.valid() {
            return Err(error::refused("the index settings are out of range"));
        }
        Ok(Self {
            pool,
            tenant,
            dim,
            index,
            runtime,
        })
    }

    /// A store that opens its own pool from `config`.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the configuration asks for TLS (this crate does not ship a
    /// TLS connector; a caller that needs TLS supplies its own pool through [`new`](Self::new)),
    /// when the pool cannot be built, or when the width or index settings are out of range.
    pub fn connect(
        config: &PgConfig,
        tenant: SiteId,
        dim: usize,
        index: IndexKind,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, RetrievalError> {
        Self::new(pool_for(config)?, tenant, dim, index, runtime)
    }

    /// Creates the `vector` extension, the table and the index if they are missing, and checks
    /// the width of an existing table. Idempotent and safe to run from several nodes at once.
    ///
    /// # Errors
    ///
    /// [`PrepareError::ExtensionMissing`] when the server has no pgvector,
    /// [`PrepareError::DimensionMismatch`] when the table has another width, and
    /// [`PrepareError::Failed`] when the database refuses.
    pub fn prepare(&self) -> Result<(), PrepareError> {
        self.block(schema::prepare_schema(&self.pool, self.dim, self.index))
    }

    /// The vector index method the table carries (`"hnsw"` or `"ivfflat"`), or `None`.
    ///
    /// # Errors
    ///
    /// [`PrepareError::Failed`] when the catalogue cannot be read.
    pub fn index_method(&self) -> Result<Option<&'static str>, PrepareError> {
        self.block(schema::index_method(&self.pool))
    }

    /// The tenant this store is bound to.
    #[must_use]
    pub fn tenant(&self) -> &SiteId {
        &self.tenant
    }

    /// The vector width this store was built for.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// How many chunks this tenant holds.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the database refuses.
    pub fn count(&self) -> Result<u64, RetrievalError> {
        self.block(async {
            let client = self.client().await?;
            ops::count(&client, self.tenant.as_str()).await
        })
    }

    /// Every chunk of one document of this tenant, in chunk order.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the database refuses or a row cannot be decoded.
    pub fn chunks_of(&self, doctype: &str, id: RecordId) -> Result<Vec<Chunk>, RetrievalError> {
        self.block(async {
            let client = self.client().await?;
            ops::chunks_of(&client, self.tenant.as_str(), doctype, id).await
        })
    }

    /// One page of the documents this tenant has chunks for, ordered by DocType (byte order)
    /// and record id, after `after` (`None` for the first page). `limit` is clamped to
    /// `1..=`[`MAX_PAGE`]; a page shorter than `limit` is the last. A reconciling sweep walks
    /// these pages and removes any document its source no longer has.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the database refuses.
    pub fn documents_page(
        &self,
        after: Option<(&str, RecordId)>,
        limit: usize,
    ) -> Result<Vec<(String, RecordId)>, RetrievalError> {
        self.block(async {
            let client = self.client().await?;
            ops::documents_page(&client, self.tenant.as_str(), after, limit).await
        })
    }

    /// Removes every chunk of each of `documents` in one transaction: the sweep that takes a
    /// batch of deleted documents out of the index together. At most [`MAX_DELETE_BATCH`].
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the batch is too large or the database refuses; nothing
    /// is removed then.
    pub fn delete_documents(&self, documents: &[(&str, RecordId)]) -> Result<(), RetrievalError> {
        self.block(async {
            let mut client = self.client().await?;
            ops::delete_documents(&mut client, self.tenant.as_str(), documents).await
        })
    }

    /// Checks out one connection from this store's pool.
    async fn client(&self) -> Result<deadpool_postgres::Client, RetrievalError> {
        self.pool
            .get()
            .await
            .map_err(|error| error::store_pool(&error))
    }

    /// Blocks on `future` on this store's runtime.
    fn block<T>(&self, future: impl Future<Output = T>) -> T {
        blocking::block_on(self.runtime.as_ref(), future)
    }
}

impl VectorStore for PgVectorStore {
    fn upsert(&self, chunks: &[Chunk]) -> Result<(), RetrievalError> {
        self.block(async {
            let mut client = self.client().await?;
            ops::upsert(&mut client, self.tenant.as_str(), self.dim, chunks).await
        })
    }

    fn delete_document(&self, doctype: &str, id: RecordId) -> Result<(), RetrievalError> {
        self.delete_documents(&[(doctype, id)])
    }

    fn search(&self, vector: &[f32], limit: usize) -> Result<Vec<ScoredChunk>, RetrievalError> {
        self.block(async {
            let mut client = self.client().await?;
            ops::search(
                &mut client,
                self.tenant.as_str(),
                self.dim,
                self.index,
                vector,
                limit,
            )
            .await
        })
    }
}

/// Builds a pool for `config` without opening a connection.
///
/// Only a plain connection is supported: `Tls::Require` needs a TLS connector this crate does not
/// depend on, and refusing it is better than silently connecting in the clear.
///
/// # Errors
///
/// [`RetrievalError::Store`] when TLS is required or the pool cannot be built.
pub fn pool_for(config: &PgConfig) -> Result<Pool, RetrievalError> {
    if config.tls == Tls::Require {
        return Err(error::refused(
            "this store does not open TLS connections; supply a pool of your own",
        ));
    }
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name(&config.application_name)
        .connect_timeout(config.connect_timeout);
    let manager = Manager::from_config(
        driver,
        NoTls,
        ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(config.pool_max_size)
        .create_timeout(Some(config.connect_timeout))
        .wait_timeout(Some(config.pool_wait_timeout))
        .runtime(PoolRuntime::Tokio1)
        .build()
        .map_err(|_| error::refused("the vector connection pool could not be built"))
}
