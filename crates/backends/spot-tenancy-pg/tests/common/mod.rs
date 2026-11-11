//! Shared setup for the tests: one scratch database, one runtime, a prepared control store and a
//! provisioner that places every tenant in that database.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as PoolRuntime};
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};
use spot_tenancy::ports::{
    ApprovalHook, Archiver, AuditEvent, AuditSink, PortResult, Ports, StepFailure,
};
use spot_tenancy::{
    Cluster, ClusterId, Config, ControlPlane, DatabaseId, ProvisionRequest, Tenant, TenantFlags,
    TenantId,
};
use spot_tenancy_pg::{Faults, PgControlStore, PgProvisioner, PlacementDb};

/// A multi-thread runtime: the store's synchronous methods call `block_in_place` from inside it
/// and the pool's connection tasks run on it.
pub fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("the test runtime builds"),
    )
}

/// A pool on `config`, recycling connections with `DISCARD ALL` so a `SET search_path` from one
/// tenant never leaks into the next checkout.
pub fn pool(config: &PgConfig, max_size: usize) -> Pool {
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-tenancy-pg-test");
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Clean,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(PoolRuntime::Tokio1)
        .build()
        .expect("the pool builds")
}

/// The fake final-backup port. It is not the point of this task; the control plane only needs a
/// verified reference for the deprovisioning test.
pub struct TestArchiver {
    verify_ok: AtomicBool,
}

impl Default for TestArchiver {
    fn default() -> Self {
        Self {
            verify_ok: AtomicBool::new(true),
        }
    }
}

impl TestArchiver {
    pub fn set_verify(&self, ok: bool) {
        self.verify_ok.store(ok, Ordering::SeqCst);
    }
}

impl Archiver for TestArchiver {
    fn final_backup(&self, tenant: &Tenant) -> PortResult<String> {
        Ok(format!("backup-{}", tenant.id))
    }

    fn verify(&self, _tenant: &Tenant, _backup_ref: &str) -> PortResult<bool> {
        Ok(self.verify_ok.load(Ordering::SeqCst))
    }
}

/// The fake two-person approval hook.
#[derive(Default)]
pub struct TestApprovals {
    approvers: Mutex<Vec<String>>,
}

impl TestApprovals {
    pub fn set(&self, approvers: &[&str]) {
        *self.approvers.lock().expect("approvals lock") =
            approvers.iter().map(|name| (*name).to_owned()).collect();
    }
}

impl ApprovalHook for TestApprovals {
    fn approvers(&self, _tenant: &Tenant) -> Vec<String> {
        self.approvers.lock().expect("approvals lock").clone()
    }
}

/// The recording audit sink.
#[derive(Default)]
pub struct TestAudit {
    events: Mutex<Vec<AuditEvent>>,
}

impl TestAudit {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("audit lock").clone()
    }
}

impl AuditSink for TestAudit {
    fn record(&self, event: &AuditEvent) -> PortResult {
        self.events.lock().expect("audit lock").push(event.clone());
        Ok(())
    }
}

/// Everything a test needs, on one scratch database.
pub struct Fixture {
    pub db: TestDb,
    pub rt: Arc<tokio::runtime::Runtime>,
    pub faults: Arc<Faults>,
    pub store: PgControlStore,
    pub provisioner: PgProvisioner,
    pub clusters: Vec<Cluster>,
    pub cluster: ClusterId,
    pub database: DatabaseId,
    pub archiver: TestArchiver,
    pub approvals: TestApprovals,
    pub audit: TestAudit,
}

impl Fixture {
    /// A control plane over this fixture. Build one per thread: the ports are trait objects and
    /// are not themselves `Sync`.
    pub fn plane(&self) -> ControlPlane<'_> {
        ControlPlane {
            store: &self.store,
            ports: Ports {
                schema: &self.provisioner,
                seeder: &self.provisioner,
                iam: &self.provisioner,
                licence: &self.provisioner,
                telemetry: &self.provisioner,
            },
            archiver: &self.archiver,
            approvals: &self.approvals,
            audit: &self.audit,
            clusters: &self.clusters,
            config: Config::default(),
        }
    }

    /// Provisions and runs one tenant, asserting both succeed.
    pub fn provision(&self, name: &str, entropy: u8, now: u64) -> TenantId {
        let plane = self.plane();
        let plan = plane
            .provision(&request(name, "eu-west", entropy), "ops", now)
            .expect("provision plans");
        plane
            .run(&plan.tenant, "ops", now + 1)
            .expect("provision runs");
        plan.tenant
    }

    /// The seed row count of a tenant's schema, read on a raw connection with that schema first
    /// in `search_path`.
    pub fn seed_rows(&self, tenant: &TenantId) -> i64 {
        let schema = tenant.schema_name();
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .batch_execute(&format!("SET search_path TO \"{schema}\""))
                .await
                .expect("select the tenant schema");
            let row = client
                .query_one("SELECT count(*) FROM tenant_seed", &[])
                .await
                .expect("count the seed rows");
            row.get(0)
        })
    }

    /// Whether the tenant's schema still exists in the placement database.
    pub fn schema_exists(&self, tenant: &TenantId) -> bool {
        let schema = tenant.schema_name();
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .query_one("SELECT to_regnamespace($1) IS NOT NULL", &[&schema])
                .await
                .expect("read the schema")
                .get(0)
        })
    }

    /// The labels of the seed rows visible with this tenant's schema in `search_path`.
    pub fn seed_labels(&self, tenant: &TenantId) -> Vec<String> {
        let schema = tenant.schema_name();
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .batch_execute(&format!("SET search_path TO \"{schema}\""))
                .await
                .expect("select the tenant schema");
            client
                .query("SELECT label FROM tenant_seed ORDER BY label", &[])
                .await
                .expect("read the seed rows")
                .iter()
                .map(|row| row.get(0))
                .collect()
        })
    }
}

/// A scratch database with a prepared control store and a provisioner, or `None` (printing
/// `SKIPPED`) when `SPOT_TEST_PG_URL` is unset.
pub async fn fixture(rt: Arc<tokio::runtime::Runtime>) -> Option<Fixture> {
    let db = TestDb::create().await?;
    let faults = Arc::new(Faults::new());
    let store = PgControlStore::new(pool(&db.config(), 8), Arc::clone(&rt), Arc::clone(&faults));
    store
        .prepare()
        .await
        .expect("the control schema is created");

    let cluster = ClusterId::new("c1").expect("a cluster id");
    let database = DatabaseId::new(db.name()).expect("a database id");
    let placement = PlacementDb {
        cluster: cluster.clone(),
        database: database.clone(),
        config: db.config(),
        pool: pool(&db.config(), 16),
    };
    let app_catalog = Catalog::new(1, Collation::new("en").expect("a collation"), vec![])
        .expect("an empty catalogue");
    let provisioner = PgProvisioner::new(
        db.config(),
        vec![placement],
        app_catalog,
        Arc::clone(&rt),
        Arc::clone(&faults),
    )
    .expect("the provisioner builds");
    let clusters = vec![Cluster {
        cluster: cluster.clone(),
        database: database.clone(),
        region: spot_tenancy::Region::new("eu-west").expect("a region"),
        capacity: 100,
        accepting: true,
    }];
    Some(Fixture {
        db,
        rt,
        faults,
        store,
        provisioner,
        clusters,
        cluster,
        database,
        archiver: TestArchiver::default(),
        approvals: TestApprovals::default(),
        audit: TestAudit::default(),
    })
}

/// A provisioning request with a caller-supplied entropy byte.
pub fn request(name: &str, region: &str, entropy: u8) -> ProvisionRequest {
    let mut bytes = [0u8; 10];
    bytes[0] = entropy;
    bytes[9] = entropy.wrapping_mul(7);
    ProvisionRequest {
        name: name.to_owned(),
        region: region.to_owned(),
        edition: "standard".to_owned(),
        entropy: bytes,
        flags: TenantFlags::NONE,
    }
}

/// A failure a test may want to inspect.
pub fn failure_reason(result: std::result::Result<(), StepFailure>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(failure) => failure.reason,
    }
}
