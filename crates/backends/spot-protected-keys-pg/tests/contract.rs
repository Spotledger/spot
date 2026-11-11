//! The `KeyStore` contract suite (the file `spot-protected-keys/tests/support/contract.rs`,
//! compiled in here with `#[path]`) on a real PostgreSQL server.
//!
//! With `SPOT_REQUIRE_PG=1` a missing server is a failure, so CI cannot go green by skipping.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

#[path = "../../../framework/spot-protected-keys/tests/support/contract.rs"]
#[rustfmt::skip]
#[allow(clippy::all, clippy::pedantic)]
mod contract;

use std::sync::Arc;

use deadpool_postgres::Pool;
use spot_pg_bridge::Bridge;
use spot_postgres::PgConfig;
use spot_protected_keys::KeyStore;
use spot_protected_keys_pg::PgKeyStore;

struct Postgres {
    bridge: Arc<Bridge>,
    pool: Pool,
}

impl contract::Harness for Postgres {
    fn open(&self, tenant: &str) -> Arc<dyn KeyStore> {
        Arc::new(PgKeyStore::new(Arc::clone(&self.bridge), &self.pool, tenant).unwrap())
    }
}

fn harness(config: PgConfig) -> Postgres {
    let (bridge, pool, _store) = common::open_store(config, "setup");
    Postgres { bridge, pool }
}

#[test]
fn the_contract_suite_passes_on_postgres() {
    let Some(fx) = common::fixture() else { return };
    contract::run_all(&harness(fx.db.config()));
}

#[test]
fn every_check_passes_on_its_own_database_too() {
    // A second pass on a fresh database proves no check leans on rows another one left behind.
    let Some(fx) = common::fixture() else { return };
    let h = harness(fx.db.config());
    contract::restart_loses_nothing(&h);
    contract::rotation_race_has_one_winner(&h);
    contract::revocation_races_release(&h);
}
