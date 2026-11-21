//! Shared setup: a scratch database with the vector extension and the retrieval table, and
//! stores bound to tenants on it.
//!
//! Every test skips cleanly (printing one `SKIPPED:` line) when `SPOT_TEST_PG_URL` is unset, and
//! when the server has no `vector` extension unless `SPOT_REQUIRE_PG=1`, in which case a missing
//! extension is a failure, so CI cannot go green by skipping.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_core::SiteId;
use spot_postgres::testing::TestDb;
use spot_retrieval_pg::{IndexKind, PgVectorStore, PrepareError, pool_for};

/// The width every store in the tests uses unless a test says otherwise.
pub const DIM: usize = spot_retrieval_pg::conformance::DIM;

/// A multi-thread runtime: the store's synchronous methods block on it (and call
/// `block_in_place` from inside it), and the pool's connection tasks run on it.
pub fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("the test runtime builds"),
    )
}

pub fn tenant(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn require_pg() -> bool {
    std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1")
}

/// A scratch database whose retrieval table is prepared.
pub struct Fixture {
    pub db: TestDb,
    pub rt: Arc<tokio::runtime::Runtime>,
}

impl Fixture {
    /// A store bound to `tenant_name` on the scratch database, over the prepared table.
    pub fn store(&self, tenant_name: &str) -> PgVectorStore {
        self.store_with(tenant_name, DIM, IndexKind::Hnsw)
    }

    /// A store with an explicit width and index kind. It does not prepare the table.
    pub fn store_with(&self, tenant_name: &str, dim: usize, kind: IndexKind) -> PgVectorStore {
        let pool = pool_for(&self.db.config()).expect("a pool on the scratch database");
        PgVectorStore::new(pool, tenant(tenant_name), dim, kind, Arc::clone(&self.rt))
            .expect("the store builds")
    }

    /// A plain driver client on the scratch database, for catalogue checks.
    pub fn client(&self) -> tokio_postgres::Client {
        self.rt.block_on(self.db.raw_client())
    }
}

/// A prepared scratch database, or `None` after printing `SKIPPED:`.
pub fn fixture() -> Option<Fixture> {
    let rt = runtime();
    let db = rt.block_on(TestDb::create())?;
    let fixture = Fixture { db, rt };
    match fixture.store("setup").prepare() {
        Ok(()) => Some(fixture),
        Err(PrepareError::ExtensionMissing) => {
            if require_pg() {
                panic!("SPOT_REQUIRE_PG=1 but the server has no pgvector extension");
            }
            println!("SKIPPED: the test server has no pgvector extension");
            None
        }
        Err(error) => panic!("the retrieval table could not be created: {error}"),
    }
}
