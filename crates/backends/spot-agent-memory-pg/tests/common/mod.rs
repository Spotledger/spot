//! Shared setup for the tests: a scratch database as a memory database, a store on it, and a
//! provisioned tenant with its own database and role on the same server.
//!
//! Every test skips cleanly (printing one `SKIPPED:` line) when `SPOT_TEST_PG_URL` is unset, and
//! when the server has no `vector` extension unless `SPOT_REQUIRE_PG=1` — in which case a missing
//! extension is a failure, so CI cannot go green by skipping.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_agent_memory::{Entry, MemoryStore, TenantId};
use spot_agent_memory_pg::{PgMemoryProvisioner, PgMemoryStore};
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;

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

/// A tenant id, asserting it is valid.
pub fn tenant(name: &str) -> TenantId {
    TenantId::parse(name).unwrap_or_else(|error| panic!("the test tenant must be valid: {error}"))
}

/// Whether a missing `vector` extension is a failure here (CI) or a skip (a plain dev run).
fn require_pg() -> bool {
    std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1")
}

/// A store over a prepared scratch database, or `None` after printing `SKIPPED:`.
pub fn store(
    db: &TestDb,
    rt: &Arc<tokio::runtime::Runtime>,
    tenant: &TenantId,
) -> Option<PgMemoryStore> {
    // `connect` insists on the tenant's derived database name; a scratch database has another, so
    // the fixtures hand a pool to `new`.
    let pool = spot_agent_memory_pg::pool_for(&db.config()).expect("a pool on the scratch db");
    let store = PgMemoryStore::new(pool, tenant.clone(), Arc::clone(rt)).expect("the store builds");
    match store.prepare() {
        Ok(()) => Some(store),
        Err(error) if error.code == spot_agent_memory_pg::EXTENSION_MISSING => {
            if require_pg() {
                panic!("SPOT_REQUIRE_PG=1 but the server has no pgvector extension");
            }
            println!("SKIPPED: the test server has no pgvector extension");
            None
        }
        Err(error) => panic!("the memory tables could not be created: {error:?}"),
    }
}

/// A scratch database and a store on it, for one tenant.
pub struct Fixture {
    pub db: TestDb,
    pub rt: Arc<tokio::runtime::Runtime>,
    pub store: PgMemoryStore,
    pub tenant: TenantId,
}

/// A fixture for `tenant_name`, or `None` when the database is unavailable or not prepared.
pub fn fixture(tenant_name: &str) -> Option<Fixture> {
    let rt = runtime();
    let db = rt.block_on(TestDb::create())?;
    let tenant = tenant(tenant_name);
    let store = store(&db, &rt, &tenant)?;
    Some(Fixture {
        db,
        rt,
        store,
        tenant,
    })
}

/// The last `chars` characters of the scratch database's UUID: the random part of the UUIDv7, which
/// differs between databases created in the same millisecond (its leading bits are a timestamp).
fn uuid_tail(db: &TestDb, chars: usize) -> String {
    let name = db.name().trim_start_matches("spot_test_");
    let start = name.len().saturating_sub(chars);
    name.chars().skip(start).collect()
}

/// A tenant whose id is unique to this run, so its memory database on the shared test server is
/// unique too.
pub fn ephemeral_tenant(db: &TestDb) -> TenantId {
    tenant(&format!("tenant_{}", uuid_tail(db, 12)))
}

/// A business database and role, and one table in it: the tenant's business data, which memory
/// must never reach.
pub struct Business {
    pub database: String,
    pub role: String,
    pub password: String,
}

impl Business {
    /// A table name no memory table shares.
    pub const TABLE: &'static str = "business_job";

    /// Creates the business database, its role and one table, through the admin connection.
    ///
    /// # Panics
    /// When the server refuses (the test harness must be loud).
    pub fn create(rt: &Arc<tokio::runtime::Runtime>, admin: &PgConfig, db: &TestDb) -> Self {
        let suffix = uuid_tail(db, 10);
        let business = Self {
            database: format!("spot_biz_{suffix}"),
            role: format!("spot_biz_role_{suffix}"),
            password: "business-role-secret-0001".to_owned(),
        };
        rt.block_on(async {
            spot_postgres::create_database(admin, &business.database, "und").await
        })
        .expect("the business database is created");
        let admin_client = rt.block_on(async { db.admin_client().await });
        rt.block_on(async {
            admin_client
                .batch_execute(&format!(
                    "CREATE ROLE {} LOGIN PASSWORD '{}';\n\
                     GRANT CONNECT ON DATABASE {} TO {}",
                    business.role, business.password, business.database, business.role
                ))
                .await
                .expect("the business role is created");
        });
        let business_config = admin.clone().with_dbname(&business.database);
        let client = rt.block_on(async {
            let pool = spot_agent_memory_pg::pool_for(&business_config).expect("a pool");
            pool.get().await.expect("the business database connection")
        });
        rt.block_on(async {
            client
                .batch_execute(&format!(
                    "CREATE TABLE {} (id text PRIMARY KEY, amount numeric(20, 0) NOT NULL);\n\
                     INSERT INTO {} (id, amount) VALUES ('job-1', 12500);\n\
                     REVOKE ALL ON {} FROM PUBLIC",
                    Self::TABLE,
                    Self::TABLE,
                    Self::TABLE
                ))
                .await
                .expect("the business table is created");
        });
        business
    }

    /// The business role's connection configuration.
    #[must_use]
    pub fn config(&self, admin: &PgConfig) -> PgConfig {
        let mut config = admin.clone().with_dbname(&self.database);
        config.user.clone_from(&self.role);
        self.password.clone_into(&mut config.password);
        config
    }

    /// Drops the database and the role, best effort.
    pub fn drop_all(&self, rt: &Arc<tokio::runtime::Runtime>, db: &TestDb) {
        let client = rt.block_on(async { db.admin_client().await });
        rt.block_on(async {
            let _ = client
                .batch_execute(&format!(
                    "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                    self.database
                ))
                .await;
            let _ = client
                .batch_execute(&format!("DROP ROLE IF EXISTS \"{}\"", self.role))
                .await;
        });
    }
}

/// A provisioned memory database that is dropped again when the test ends, so a shared test
/// server does not fill up with the databases of earlier runs.
pub struct Provisioned {
    provisioner: Arc<PgMemoryProvisioner>,
    pub tenant: TenantId,
}

impl Provisioned {
    /// Provisions `tenant`'s memory database.
    pub fn new(provisioner: Arc<PgMemoryProvisioner>, tenant: TenantId, password: &str) -> Self {
        provisioner
            .prepare_control()
            .expect("the control record table is created");
        provisioner
            .provision(&tenant, password, 1_000)
            .expect("the memory database is provisioned");
        Self {
            provisioner,
            tenant,
        }
    }
}

impl Drop for Provisioned {
    /// Deprovisions on a short-lived thread with its own runtime: the test's runtime is often
    /// shutting down when this runs.
    fn drop(&mut self) {
        let provisioner = Arc::clone(&self.provisioner);
        let tenant = self.tenant.clone();
        let job = std::thread::spawn(move || {
            let _ = provisioner.deprovision(&tenant);
        });
        let _ = job.join();
    }
}

/// A client on the scratch database: where this crate's control store record lives (the
/// provisioner is given the scratch database's configuration as its control connection).
pub fn control_client(rt: &Arc<tokio::runtime::Runtime>, db: &TestDb) -> deadpool_postgres::Client {
    let config = db.config();
    let pool = spot_agent_memory_pg::pool_for(&config).expect("a pool on the control database");
    rt.block_on(pool.get()).expect("the control connection")
}

/// The entries a store holds, by kind and id, for an equality assertion that reads through the
/// trait rather than this crate's own accessors.
pub fn snapshot(store: &PgMemoryStore, tenant: &TenantId) -> Vec<Entry> {
    let mut entries = Vec::new();
    for kind in spot_agent_memory::MemoryKind::all() {
        entries.extend(store.list(tenant, kind));
    }
    entries
}
