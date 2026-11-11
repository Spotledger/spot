#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]
//! The PostgreSQL + `pgvector` agent memory store (WP-AI-09.T2, WS13, ADR-26).
//!
//! `spot-agent-memory` is the model and the logic: three kinds of memory (episodes, procedures,
//! deployment facts) behind the object-safe [`MemoryStore`] trait, a sanitiser that keeps business
//! data out of the memory namespace, recall, retention and the learning loop, plus a deterministic
//! in-memory store. [`PgMemoryStore`] is the durable one. It implements the same five calls over
//! PostgreSQL with `pgvector` embeddings, and adds the two things a database can do that a
//! `BTreeMap` cannot: rank a recall inside the database ([`recall`]) and keep a tenant's memory in
//! a database of its own ([`provision`]).
//!
//! # Memory lives in its own database
//!
//! WS13's acceptance is that agent memory is **separate from the tenant's business data with no
//! cross-database access path**, so this crate never writes a memory table into a business
//! database and never opens a connection to one:
//!
//! - [`PgMemoryProvisioner`] creates a **database and a role per tenant** on the control server
//!   (`spot_memory_<derived>`, `spot_memory_role_<derived>`, derived from the tenant id and never
//!   from a tenant-chosen name), makes the role the database's owner, revokes `CONNECT` from
//!   `PUBLIC`, installs the `vector` extension there and records the placement in the control
//!   store. [`PgMemoryProvisioner::deprovision`] drops the database, the role and the record.
//! - `CREATE EXTENSION vector` runs in the memory database and nowhere else, and
//!   [`PgMemoryStore::prepare`] creates the tables there.
//! - A [`PgMemoryStore`] holds exactly one pool, on one memory database, bound to one tenant, and
//!   is built by `connect` only for the tenant's derived memory database name. It opens
//!   no connection to a business database and has no statement that names a business table. The
//!   memory role is not *granted* anything on a business database, but it can connect to one
//!   unless the control plane revokes `PUBLIC CONNECT` there; the isolation tests connect as
//!   each role and try.
//!
//! # What the store keeps
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_memory_episode` | what happened |
//! | `spot_memory_procedure` | how to fix it |
//! | `spot_memory_fact` | what the installation is |
//! | `spot_memory_candidate` | the candidate-remedy queue, with a `CHECK` that a `promoted` row carries a non-null `ci_run` (it does not prove the run exists or passed) |
//!
//! See [`schema`] for the columns, the constraints and the recall index.
//!
//! # Rules the store keeps
//!
//! - **One tenant per store.** Every statement filters on the store's tenant, and a call naming
//!   another tenant is refused (write) or reads as absent (read) before the database is touched.
//! - **Decoding goes through the domain constructors.** A stored row is a place values are kept,
//!   not a trusted source: ids are re-parsed, free text is re-scanned by the
//!   [`Sanitiser`](spot_agent_memory::Sanitiser), jsonb documents are rebuilt through the entry
//!   types' own rules, and a row that no longer satisfies its kind's `validate()` is refused as
//!   `agent_memory.store_failed` naming the column — never the value.
//! - **Values are bound, never formatted.** Every statement in this crate is a `const` with `$n`
//!   placeholders. The only text that reaches SQL is a table name, a column list or an index name
//!   written in this crate, and an integer the planner chose.
//! - **No clock, no float, no secret.** Time arrives as `now_ms`; the one rounding of a cosine
//!   happens in SQL beside the extension; nothing logs, and no returned type carries the
//!   connection string or a password ([`PgMemoryStore`]'s `Debug` prints the tenant and the pool
//!   size only).
//!
//! # Tests and the database
//!
//! The integration tests need PostgreSQL with `pgvector` and skip cleanly (printing `SKIPPED:`)
//! when `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do. A server *without* pgvector is
//! reported as `agent_memory_pg.extension_missing`, which the tests turn into a `SKIPPED:` line:
//! CI sets `SPOT_TEST_PG_URL` and `SPOT_REQUIRE_PG=1`, so a missing extension fails loudly there
//! instead of passing quietly.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Pool, Runtime as PoolRuntime};
use spot_agent_memory::{
    CandidateId, Entry, MemoryError, MemoryKind, RecallHit, Sanitiser, TenantExport, TenantId,
    export_tenant,
};
use spot_app_api::AppResult;
use spot_postgres::{PgConfig, Tls};
use tokio_postgres::NoTls;

mod blocking;
mod candidate;
mod codec;
mod error;
mod provision;
mod recall;
mod row;
pub mod schema;
mod wire;

pub use candidate::MAX_CANDIDATES;
pub use candidate::StoredCandidate;
pub use error::{EXTENSION_MISSING, MIGRATION_FAILED, PROVISION_FAILED};
pub use provision::{MemoryPlacement, PgMemoryProvisioner};
pub use recall::{MAX_BOOST_BP, MAX_RECALL_K, MAX_WINDOW_MS, RecallQuery, RecencyBoost};
pub use row::{MAX_PAGE, MAX_TENANTS, MAX_TIME_MS, MAX_TTL_MS};
pub use schema::{IVFFLAT_FROM_ROWS, RecallIndex, VECTOR_DIM};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-memory-pg";

/// The [`MemoryStore`](spot_agent_memory::MemoryStore) backed by the tenant's memory database.
pub struct PgMemoryStore {
    pool: Pool,
    tenant: TenantId,
    sanitiser: Sanitiser,
    runtime: Arc<tokio::runtime::Runtime>,
    max_entries: usize,
}

/// The default most entries one kind may hold for the tenant: the domain retention default.
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

impl fmt::Debug for PgMemoryStore {
    /// Names the tenant and the pool size. A pool's configuration can carry a password, so it is
    /// never printed: a `Debug` line ends up in a log, and a log is not a place a secret may be.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMemoryStore")
            .field("tenant", &self.tenant)
            .field("pool_size", &self.pool.status().max_size)
            .finish_non_exhaustive()
    }
}

impl PgMemoryStore {
    /// A store over `pool`, for `tenant`, blocking on `runtime`.
    ///
    /// The pool must already point at that tenant's memory database: this type opens no other
    /// connection, which is what keeps memory out of the business database. A pool's
    /// configuration cannot be read back, so this constructor trusts the caller; use
    /// [`connect`](Self::connect), which checks the database name, or call
    /// [`verify_database`](Self::verify_database) to ask the server which database the pool
    /// reaches.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] when the standard sanitiser cannot be built (a compiled-in pattern
    /// is broken, which no value from a caller can cause).
    pub fn new(
        pool: Pool,
        tenant: TenantId,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, MemoryError> {
        let sanitiser = Sanitiser::standard().map_err(|_| MemoryError::Store {
            detail: "the memory sanitiser could not be built".to_owned(),
        })?;
        Ok(Self {
            pool,
            tenant,
            sanitiser,
            runtime,
            max_entries: DEFAULT_MAX_ENTRIES,
        })
    }

    /// The most entries one kind (episode, procedure, fact) may hold for this tenant; a `put` of
    /// a new id beyond it is refused with `agent_memory.store_failed` until retention frees
    /// room. The default is [`DEFAULT_MAX_ENTRIES`]. T1's `MemoryStore` trait carries no
    /// capacity, so the cap is enforced here, inside the PostgreSQL store's `put`.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when `max_entries` is zero.
    pub fn with_max_entries(mut self, max_entries: usize) -> Result<Self, MemoryError> {
        if max_entries == 0 {
            return Err(MemoryError::Store {
                detail: "a memory store holds at least one entry per kind".to_owned(),
            });
        }
        self.max_entries = max_entries;
        Ok(self)
    }

    /// A store that opens its own pool from `config`, for `tenant`.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] when the configuration asks for TLS (this crate does not ship a TLS
    /// connector: a memory database is reached over the control plane's private network, and a
    /// caller that needs TLS supplies its own pool through [`new`](Self::new)), when the pool
    /// cannot be built, when the sanitiser cannot be built, or when `config.dbname` is not the
    /// tenant's derived memory database.
    pub fn connect(
        config: &PgConfig,
        tenant: TenantId,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, MemoryError> {
        if config.dbname != provision::placement_for(&tenant).database {
            return Err(wrong_database());
        }
        Self::new(pool_for(config)?, tenant, runtime)
    }

    /// Creates the extension, the tables and the recall index in this tenant's memory database.
    ///
    /// # Errors
    /// `agent_memory_pg.extension_missing` when the server has no `vector` extension, and
    /// `agent_memory_pg.migration_failed` when the database cannot apply the DDL.
    pub fn prepare(&self) -> AppResult<()> {
        self.block(schema::prepare(&self.pool))
    }

    /// Checks that the pool reaches this tenant's derived memory database
    /// ([`placement_for`](provision::placement_for)), by asking the server.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the connected database is any other, or the server refuses.
    pub fn verify_database(&self) -> Result<(), MemoryError> {
        self.block(async {
            let client = self.client().await?;
            let row = client
                .query_one("SELECT current_database()::text", &[])
                .await
                .map_err(|error| error::store(&error))?;
            let name: String = row.try_get(0).map_err(|_| wrong_database())?;
            if name == provision::placement_for(&self.tenant).database {
                Ok(())
            } else {
                Err(wrong_database())
            }
        })
    }

    /// The tenant this store is bound to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// How many entries of `kind` this tenant's memory holds.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses.
    pub fn count(&self, kind: MemoryKind) -> Result<i64, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::count(&client, &self.tenant, kind).await
        })
    }

    /// One entry of `kind`, reporting a database failure instead of reading it as absence.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn try_get(&self, kind: MemoryKind, id: &str) -> Result<Option<Entry>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::get(&client, &self.tenant, kind, id, &self.sanitiser).await
        })
    }

    /// Every entry of `kind` (at most `max_entries`, read in pages), ordered by id, reporting a database failure instead of reading it
    /// as emptiness.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn try_list(&self, kind: MemoryKind) -> Result<Vec<Entry>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::list(
                &client,
                &self.tenant,
                kind,
                self.max_entries,
                &self.sanitiser,
            )
            .await
        })
    }

    /// One page of entries of `kind`, ordered by id, after `after_id` (`None` for the first
    /// page). `limit` is clamped to `1..=`[`MAX_PAGE`]; a page shorter than `limit` is the last,
    /// and the last id of a page is the next `after_id`.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn list_page(
        &self,
        kind: MemoryKind,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entry>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::list_page(
                &client,
                &self.tenant,
                kind,
                after_id,
                limit,
                &self.sanitiser,
            )
            .await
        })
    }

    /// One page of the tenants the tables hold rows for, ordered, after `after`.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses.
    pub fn tenants_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TenantId>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::tenants_page(&client, after, limit).await
        })
    }

    /// One page of the candidate queue, in id order, after `after_id`.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn candidates_page(
        &self,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredCandidate>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            candidate::list_page(&client, &self.tenant, after_id, limit).await
        })
    }

    /// Deletes this tenant's entries whose time to live has run out at `now_ms`, `batch` rows
    /// per statement (clamped to `1..=`[`MAX_PAGE`]) until none are left, and returns how many
    /// were removed.
    ///
    /// This is the hard TTL only. It does not know a legal hold, so a host that holds entries
    /// plans retention through `spot_agent_memory` (which reads this store through
    /// [`try_list`](Self::try_list)) instead of calling this.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses.
    pub fn purge_expired(&self, now_ms: i64, batch: usize) -> Result<u64, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            row::purge_expired(&client, &self.tenant, now_ms, batch).await
        })
    }

    /// Hybrid recall: vector similarity, the structured filters and the recency boost, ranked in
    /// one statement (see [`recall`]).
    ///
    /// # Errors
    /// [`MemoryError::EntryInvalid`] when the query vector is not
    /// [`EMBEDDING_DIM`](spot_agent_memory::EMBEDDING_DIM) finite values, and
    /// [`MemoryError::Store`] when the request names another tenant or the database refuses.
    pub fn recall(&self, query: &RecallQuery<'_>) -> Result<Vec<RecallHit>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            recall::recall(&client, &self.tenant, query, VECTOR_DIM).await
        })
    }

    /// `EXPLAIN (FORMAT TEXT)` of the recall statement with `query`'s parameters, as the server
    /// rendered it.
    ///
    /// # Errors
    /// As [`recall`](Self::recall).
    pub fn recall_plan(&self, query: &RecallQuery<'_>) -> Result<String, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            recall::plan(&client, query, VECTOR_DIM).await
        })
    }

    /// Everything this tenant's memory holds, as the domain model's export.
    ///
    /// This is the export a deletion needs first: `spot_agent_memory::delete_tenant` and this
    /// crate's [`PgMemoryProvisioner::deprovision`] remove the data for good, and only an export
    /// kept somewhere else survives them.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses, and [`MemoryError::Serialise`]
    /// when a record cannot be rendered.
    pub fn export(&self) -> Result<TenantExport, MemoryError> {
        export_tenant(self, &self.tenant)
    }

    /// Records a candidate remedy in the queue, replacing the row of the same id.
    ///
    /// The candidate's own transitions are the learning loop's business
    /// (`spot_agent_memory::CandidateRemedy`); this crate stores the result. The table's `CHECK`
    /// refuses a `Promoted` row that names no CI run, so a promotion the domain type could not
    /// have produced cannot be written here either.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses.
    pub fn record_candidate(
        &self,
        candidate: &spot_agent_memory::CandidateRemedy,
        updated_ms: i64,
    ) -> Result<(), MemoryError> {
        self.block(async {
            let client = self.client().await?;
            candidate::put(&client, &self.tenant, candidate, updated_ms).await
        })
    }

    /// The queued candidate with `id`, as the row records it.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn candidate(&self, id: &CandidateId) -> Result<Option<StoredCandidate>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            candidate::get(&client, &self.tenant, id).await
        })
    }

    /// Every queued candidate, in id order.
    ///
    /// # Errors
    /// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
    pub fn candidates(&self) -> Result<Vec<StoredCandidate>, MemoryError> {
        self.block(async {
            let client = self.client().await?;
            candidate::list(&client, &self.tenant).await
        })
    }

    /// Checks out one connection from this store's pool.
    async fn client(&self) -> Result<deadpool_postgres::Client, MemoryError> {
        self.pool
            .get()
            .await
            .map_err(|error| error::store_pool(&error))
    }

    /// Blocks on `future` on this store's runtime.
    fn block<T>(&self, future: impl Future<Output = T>) -> T {
        blocking::block_on(self.runtime.as_ref(), future)
    }

    /// Whether `tenant` is the tenant this store is bound to.
    fn owns(&self, tenant: &TenantId) -> bool {
        *tenant == self.tenant
    }
}

impl spot_agent_memory::MemoryStore for PgMemoryStore {
    fn put(&self, entry: Entry) -> Result<(), MemoryError> {
        if !self.owns(&entry.meta().tenant) {
            return Err(another_tenant());
        }
        self.block(async {
            let client = self.client().await?;
            row::ensure_room(
                &client,
                &self.tenant,
                entry.kind(),
                entry.id_str(),
                self.max_entries,
            )
            .await?;
            row::put(&client, &entry).await
        })
    }

    /// A database failure reads as absence. Use [`try_get`](Self::try_get) where the difference
    /// matters — a store that cannot answer is a store nobody should act on.
    fn get(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Option<Entry> {
        if !self.owns(tenant) {
            return None;
        }
        self.try_get(kind, id).ok().flatten()
    }

    /// A database failure reads as emptiness; see [`get`](Self::get).
    fn list(&self, tenant: &TenantId, kind: MemoryKind) -> Vec<Entry> {
        if !self.owns(tenant) {
            return Vec::new();
        }
        self.try_list(kind).unwrap_or_default()
    }

    fn remove(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Result<bool, MemoryError> {
        if !self.owns(tenant) {
            return Err(another_tenant());
        }
        self.block(async {
            let client = self.client().await?;
            row::remove(&client, &self.tenant, kind, id).await
        })
    }

    /// A database failure reads as emptiness; see [`get`](Self::get).
    fn tenants(&self) -> Vec<TenantId> {
        self.block(async {
            let client = match self.client().await {
                Ok(client) => client,
                Err(_) => return Vec::new(),
            };
            row::tenants(&client).await.unwrap_or_default()
        })
    }
}

/// The pool is connected to a database that is not this tenant's memory database.
fn wrong_database() -> MemoryError {
    MemoryError::Store {
        detail: "this memory store is not connected to its tenant's memory database".to_owned(),
    }
}

/// A store bound to another tenant refused the call.
fn another_tenant() -> MemoryError {
    MemoryError::Store {
        detail: "this memory store is bound to another tenant".to_owned(),
    }
}

/// Builds a pool for `config` without opening a connection.
///
/// Only a plain connection is supported: `Tls::Require` needs a TLS connector this crate does not
/// depend on, and refusing it is better than silently connecting in the clear.
///
/// # Errors
///
/// [`MemoryError::Store`] when TLS is required or the pool cannot be built.
pub fn pool_for(config: &PgConfig) -> Result<Pool, MemoryError> {
    if config.tls == Tls::Require {
        return Err(MemoryError::Store {
            detail: "this store does not open TLS connections; supply a pool of your own"
                .to_owned(),
        });
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
    let runtime = PoolRuntime::Tokio1;
    Pool::builder(manager)
        .max_size(config.pool_max_size)
        .create_timeout(Some(config.connect_timeout))
        .wait_timeout(Some(config.pool_wait_timeout))
        .runtime(runtime)
        .build()
        .map_err(|_| MemoryError::Store {
            detail: "the memory connection pool could not be built".to_owned(),
        })
}
