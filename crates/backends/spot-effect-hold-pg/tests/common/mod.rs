//! Shared setup for the ledger's tests: a scratch database, a connection pool and small helpers.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_core::SiteId;
use spot_effect_hold::{
    EffectClass, EffectRef, EffectState, HoldState, MigrationId, NewEffect, ReleaseCursor, Sequence,
};
use spot_effect_hold_pg::PgLedger;
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// A pool of `size` connections whose server-side `application_name` is `app`.
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

/// A ledger on a new pool, with its tables prepared.
pub async fn ledger(db: &TestDb, size: usize, app: &str) -> PgLedger {
    let ledger = PgLedger::new(pool(db, size, app));
    ledger.prepare().await.unwrap();
    ledger
}

/// A site name, unwrapped for tests.
pub fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

/// A migration id, unwrapped for tests.
pub fn mig(name: &str) -> MigrationId {
    MigrationId::new(name).unwrap()
}

/// A held mail effect of `site` under `migration`.
pub fn effect(site: &SiteId, key: &str, created_at: i64, migration: &MigrationId) -> NewEffect {
    NewEffect {
        effect: EffectRef::new(
            site.clone(),
            EffectClass::Mail,
            "invoice_mail",
            key,
            created_at,
        ),
        migration_id: migration.clone(),
    }
}

/// A live hold of a site.
pub fn held(migration: &MigrationId, since: i64) -> HoldState {
    HoldState::Held {
        migration_id: migration.clone(),
        reason: "verify".to_owned(),
        since,
        expected_by: None,
    }
}

/// A release cursor at `dispatched` effects, the last of them `last_sequence`.
pub fn cursor(
    migration: &MigrationId,
    dispatched: u64,
    last_sequence: Option<u64>,
) -> ReleaseCursor {
    ReleaseCursor {
        migration_id: migration.clone(),
        dispatched,
        last_sequence: last_sequence.map(Sequence),
        needs_confirmation: 0,
    }
}

/// The idempotency keys of `records`, in order.
pub fn keys(records: &[spot_effect_hold::EffectRecord]) -> Vec<String> {
    records.iter().map(|record| record.key.clone()).collect()
}

/// The state text the ledger stores for `state`, for assertions that read the raw table.
pub fn state_text(state: EffectState) -> &'static str {
    match state {
        EffectState::Held => "held",
        EffectState::Releasing => "releasing",
        EffectState::Dispatched => "dispatched",
        EffectState::Discarded => "discarded",
        EffectState::NeedsConfirmation => "needs_confirmation",
    }
}
