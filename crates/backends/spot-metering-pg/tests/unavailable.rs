//! A pool that cannot connect reports `StoreUnavailable`, not a period-spec error. Needs no
//! database: the pool points at a closed local port.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{PoolConfig, Runtime};
use spot_metering::{
    Meter, MeterEvent, MeterStore, MeteringError, PeriodSpec, RetentionPolicy, TenantPeriods,
    period_of,
};
use spot_metering_pg::PgMeterStore;
use tokio_postgres::NoTls;

fn dead_store() -> PgMeterStore {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("none".to_owned());
    cfg.user = Some("none".to_owned());
    cfg.pool = Some(PoolConfig::new(1));
    PgMeterStore::new(
        cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap(),
        "acme",
    )
}

fn resolver() -> TenantPeriods {
    TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
}

fn assert_unavailable(error: &MeteringError) {
    assert!(
        matches!(error, MeteringError::StoreUnavailable { .. }),
        "expected StoreUnavailable, got {error}"
    );
    assert_eq!(error.code(), "metering.store_unavailable");
    assert!(error.is_retryable());
    let text = error.to_string();
    for forbidden in ["SQLSTATE", "127.0.0.1", "refused", "none"] {
        assert!(!text.contains(forbidden), "{text} leaks {forbidden}");
    }
}

#[tokio::test]
async fn adding_events_on_a_dead_pool_is_store_unavailable() {
    let store = dead_store();
    let event = MeterEvent::amount("e1", "acme", Meter::Transactions, 1, 0);
    let error = store
        .add_events(vec![event], &resolver())
        .await
        .unwrap_err();
    assert_unavailable(&error);
}

#[tokio::test]
async fn reading_totals_on_a_dead_pool_is_store_unavailable() {
    let store = dead_store();
    let period = period_of(0, &PeriodSpec::calendar_month("UTC").unwrap()).unwrap();
    let error = store
        .period_totals("acme", &period, &resolver())
        .await
        .unwrap_err();
    assert_unavailable(&error);
}

#[tokio::test]
async fn compaction_and_the_tombstone_count_on_a_dead_pool_are_store_unavailable() {
    let store = dead_store();
    let period = period_of(0, &PeriodSpec::calendar_month("UTC").unwrap()).unwrap();
    let error = store
        .compact(&period, &RetentionPolicy::default(), &resolver())
        .await
        .unwrap_err();
    assert_unavailable(&error);
    let error = store.tombstone_count().await.unwrap_err();
    assert_unavailable(&error);
}

#[tokio::test]
async fn another_tenant_is_refused_before_the_database_is_touched() {
    // The tenant check and the batch bound come first, so they answer correctly on a dead pool.
    let store = dead_store();
    let event = MeterEvent::amount("e1", "globex", Meter::Transactions, 1, 0);
    assert_eq!(
        store
            .add_events(vec![event], &resolver())
            .await
            .unwrap_err(),
        MeteringError::TenantMismatch
    );
    assert_eq!(store.tenant(), "acme");
}
