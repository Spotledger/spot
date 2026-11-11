//! A scratch database, a bridge on it and a store over the bridge. Nothing reads a clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_journal_pii_secrets::StoredKey;
use spot_journal_pii_secrets_pg::{PgWrappedKeyStore, ensure_present, extra_table};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_pg_migrate::{ApplyOptions, MigrationPlan, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_secrets::{KeyRef, WrappedKey};
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

/// The (empty) document catalogue the fixture migrates against: only the extra table is wanted.
pub fn empty_catalog() -> Catalog {
    Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap()
}

/// The migrator options that carry this crate's table.
pub fn options() -> PlanOptions {
    PlanOptions {
        extra_tables: vec![extra_table().expect("extra table")],
        ..PlanOptions::default()
    }
}

/// The migrator's dry run for this crate's table on the fixture's database.
pub fn plan_of(fx: &Fixture, backend: &PgBackend) -> MigrationPlan {
    fx.rt
        .block_on(dry_run(backend, &empty_catalog(), &options()))
        .expect("dry run")
}

/// Creates the table the way a host does: the migrator, with this crate's extra table.
pub fn migrate(fx: &Fixture, backend: &PgBackend) {
    let plan = plan_of(fx, backend);
    fx.rt
        .block_on(apply(backend, &plan, ApplyOptions::default()))
        .expect("apply");
}

/// A migrated store on a fresh bridge (a "node") over the fixture's database.
pub fn node(fx: &Fixture, name: &str) -> PgWrappedKeyStore {
    let backend = backend(fx, 8);
    migrate(fx, &backend);
    let bridge = bridge_of(&backend, name);
    ensure_present(&bridge).expect("table present");
    PgWrappedKeyStore::new(bridge)
}

pub fn wrapped(tag: u8) -> WrappedKey {
    WrappedKey {
        key: KeyRef {
            id: "acme-kek".to_owned(),
            version: 3,
        },
        bytes: vec![tag; 61],
    }
}

pub fn live(tag: u8) -> StoredKey {
    StoredKey::Live(wrapped(tag))
}
