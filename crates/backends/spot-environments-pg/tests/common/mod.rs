//! A scratch database, a bridge on it and a store over the bridge. Nothing reads a clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_environments::{AppRef, Environment, EnvironmentSet, Policy, Role};
use spot_environments_pg::PgEnvironmentStore;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_tenancy::{Region, TenantId};

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

pub fn bridge_config(name: &str, max_in_flight: usize) -> BridgeConfig {
    BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight,
        queue_capacity: 64,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    }
}

/// A prepared store on a fresh bridge (a "node") over the fixture's database.
pub fn node(fx: &Fixture, name: &str) -> PgEnvironmentStore {
    let backend = backend(fx, 8);
    let bridge = Arc::new(Bridge::for_backend(bridge_config(name, 6), &backend).expect("bridge"));
    let store = PgEnvironmentStore::new(bridge, &backend.pool()).expect("store");
    store.ensure_schema().expect("schema");
    store
}

/// A valid dev/test/prod set named `name`; `seed` changes the tenants.
pub fn set(name: &str, seed: u8) -> EnvironmentSet {
    let tenant = |n: u8| TenantId::new_v7(1_700_000_000_000 + u64::from(seed), [n; 10]);
    EnvironmentSet::new(
        name,
        Region::new("eu").unwrap(),
        vec![
            AppRef::new("accounts", "1.2.0").unwrap(),
            AppRef::new("stock", "0.9.1").unwrap(),
        ],
        vec![
            Environment {
                tenant: tenant(1),
                role: Role::Dev,
                order: 10,
            },
            Environment {
                tenant: tenant(2),
                role: Role::Test,
                order: 20,
            },
            Environment {
                tenant: tenant(3),
                role: Role::Prod,
                order: 30,
            },
        ],
        Policy {
            allow_skip: seed % 2 == 1,
        },
    )
    .expect("set")
}
