//! Shared setup for the tests: a scratch database, pools and receipt fixtures.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_core::SiteId;
use spot_mail::{Address, SendReceipt, SmtpReply};
use spot_mail_queue_pg::PgSentLog;
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

/// A prepared log on a new pool. A second call is how a restart (or another node) is simulated:
/// nothing but the database carries over.
pub async fn log(db: &TestDb, size: usize, app: &str) -> PgSentLog {
    let log = PgSentLog::new(pool(db, size, app));
    log.prepare().await.unwrap();
    log
}

/// A validated site id.
pub fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A parsed address.
pub fn address(value: &str) -> Address {
    value.parse().unwrap_or_else(|e| panic!("{value}: {e}"))
}

/// A receipt for `message_id` with the accepted addresses and the `(address, reply code)`
/// rejections given.
pub fn receipt(message_id: &str, accepted: &[&str], rejected: &[(&str, u16)]) -> SendReceipt {
    SendReceipt {
        message_id: message_id.to_owned(),
        accepted: accepted.iter().map(|a| address(a)).collect(),
        rejected: rejected
            .iter()
            .map(|(a, code)| (address(a), SmtpReply::new(*code, "refused for a test")))
            .collect(),
    }
}

/// A receipt with one accepted recipient and no rejection.
pub fn sent(message_id: &str) -> SendReceipt {
    receipt(message_id, &["buyer@customer.test"], &[])
}

/// How many rows the table holds.
pub async fn rows(db: &TestDb) -> i64 {
    let client = db.raw_client().await;
    let row = client
        .query_one("SELECT count(*) FROM spot_mail_sent_log", &[])
        .await
        .unwrap();
    row.get(0)
}
