//! Shared setup for the tests: a scratch database, a pool, a resolver and a small deterministic
//! random source.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_metering::{MeterStore, PeriodKey, PeriodResolver, PeriodSpec, TenantPeriods, period_of};
use spot_metering_pg::PgMeterStore;
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// Seconds from the epoch to 1970-02-01: the first instant of the second calendar month.
pub const FEBRUARY: i64 = 2_678_400;

/// A pool of `size` connections on the scratch database.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// A prepared store, bound to `tenant`, on a new pool.
pub async fn store(db: &TestDb, size: usize, tenant: &str) -> PgMeterStore {
    let store = PgMeterStore::new(pool(db, size, "metering_test"), tenant);
    store.prepare().await.unwrap();
    store
}

/// A store factory over one shared pool: what the contract suite wants, because each case gets
/// its own store and its own tenant.
pub fn factory(db: &TestDb, size: usize) -> impl Fn(&str) -> Arc<dyn MeterStore> + Send + Sync {
    let pool = pool(db, size, "metering_contract");
    move |tenant: &str| -> Arc<dyn MeterStore> { Arc::new(PgMeterStore::new(pool.clone(), tenant)) }
}

pub fn resolver() -> TenantPeriods {
    TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
}

/// The calendar-month window containing `at` (UTC).
pub fn window(at: i64) -> PeriodKey {
    period_of(at, &PeriodSpec::calendar_month("UTC").unwrap()).unwrap()
}

pub fn january() -> PeriodKey {
    window(0)
}

pub fn february() -> PeriodKey {
    window(FEBRUARY)
}

/// The resolver as a trait object, for call sites that want `&dyn PeriodResolver`.
pub fn dynr(resolver: &TenantPeriods) -> &dyn PeriodResolver {
    resolver
}

/// A xorshift generator: the tests need reproducible shuffles, not quality randomness.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}
