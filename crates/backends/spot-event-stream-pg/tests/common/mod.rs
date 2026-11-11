//! Shared setup for the tests: a scratch database, a connection pool and record builders.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_core::SiteId;
use spot_event_stream::{
    ConsumerRecord, ConsumerSpec, PayloadMode, Start, StreamError, TopicSelector,
};
use spot_event_stream_pg::PgConsumerStore;
use spot_events_contract::envelope::Position;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;
use tokio_postgres::NoTls;

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

pub fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

/// A prepared store for `site_name` on a new pool.
pub async fn store(db: &TestDb, site_name: &str, size: usize) -> PgConsumerStore {
    let store = PgConsumerStore::new(pool(db, size, "event_stream_test"), site(site_name));
    store.prepare().await.unwrap();
    store
}

/// A site name no earlier call returned, so each contract case starts empty.
pub fn fresh_site() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("case{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

pub fn spec(name: &str, partitions: u32) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: "owner".to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions,
        payload_mode: PayloadMode::Fields,
        start: Start::FromBeginning,
    }
}

pub fn instant(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds)
}

pub fn record(name: &str, partitions: u32) -> ConsumerRecord {
    ConsumerRecord::new(spec(name, partitions), instant(1_000))
}

pub fn pos(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}

/// The outcome of a store call as text: `ok` or the error's code.
pub fn outcome<T>(result: Result<T, StreamError>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => error.code().to_owned(),
    }
}
