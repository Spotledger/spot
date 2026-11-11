//! Shared setup for the tests: a scratch database, pools and prepared stores.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_access_review_pg::{PgCampaignStore, PgSodStore};
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// A pool of `size` connections to the scratch database.
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

/// Both stores on a new pool, with the schema prepared.
pub async fn stores(db: &TestDb, size: usize, app: &str) -> (PgCampaignStore, PgSodStore) {
    let pool = pool(db, size, app);
    let campaigns = PgCampaignStore::new(pool.clone());
    campaigns.prepare().await.unwrap();
    (campaigns, PgSodStore::new(pool))
}
