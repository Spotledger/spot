//! A scratch database migrated the way a host does (the migrator's extra-table hook), a bridge
//! on it and a store over the bridge. Nothing reads a clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use spot_migration_approval::DOMAIN_PREFIX;
use spot_migration_ops::{PlanHash, PlanState, StoredPlan};
use spot_migration_ops_pg::schema::extra_tables;
use spot_migration_ops_pg::{PgMigrationStore, RecordingAlert};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_pg_migrate::{ApplyOptions, MigrationPlan, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (and a printed `SKIPPED`) without `SPOT_TEST_PG_URL`.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn backend(fx: &Fixture, pool: usize) -> PgBackend {
    let mut config = fx.db.config();
    config.pool_max_size = pool;
    PgBackend::connect_unchecked(config).expect("backend")
}

pub fn bridge_of(backend: &PgBackend, name: &str) -> Arc<Bridge> {
    let config = BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight: 4,
        queue_capacity: 16,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    };
    Arc::new(Bridge::for_backend(config, backend).expect("bridge"))
}

pub fn empty_catalog() -> Catalog {
    Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap()
}

pub fn options() -> PlanOptions {
    PlanOptions {
        extra_tables: extra_tables().expect("extra tables"),
        ..PlanOptions::default()
    }
}

pub fn plan_of(fx: &Fixture, backend: &PgBackend) -> MigrationPlan {
    fx.rt
        .block_on(dry_run(backend, &empty_catalog(), &options()))
        .expect("dry run")
}

/// Creates the tables the way `spot migrate` does.
pub fn migrate(fx: &Fixture, backend: &PgBackend) -> MigrationPlan {
    let plan = plan_of(fx, backend);
    fx.rt
        .block_on(apply(backend, &plan, ApplyOptions::default()))
        .expect("apply");
    plan
}

pub struct Node {
    pub store: PgMigrationStore,
    pub alert: Arc<RecordingAlert>,
}

/// A migrated, protected store on a fresh bridge (a "node") over the fixture's database.
pub fn node(fx: &Fixture, name: &str) -> Node {
    let backend = backend(fx, 8);
    migrate(fx, &backend);
    let alert = Arc::new(RecordingAlert::new());
    let store = PgMigrationStore::new(bridge_of(&backend, name), alert.clone());
    store.ensure_present().expect("tables present");
    store.protect().expect("protect");
    Node { store, alert }
}

/// The digest the approval crate would compute over canonical text `json`.
pub fn hash_of(json: &str) -> PlanHash {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_PREFIX);
    hasher.update(json.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    PlanHash::parse(&hex).unwrap()
}

pub fn stored(json: &str, author: &str, created_at: i64) -> StoredPlan {
    StoredPlan {
        hash: hash_of(json),
        json: json.to_owned(),
        author: author.to_owned(),
        created_at,
        state: PlanState::Proposed,
        version: 1,
    }
}
