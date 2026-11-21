//! WP-AI-07.T18: `spot_job_requeued_total` reaches the host's Prometheus recorder. A requeue
//! through the `JobAdmin` port over the store the host builds (the in-memory profile's
//! `Settings::memory_store`, and on PostgreSQL `durable_store`) counts once under the job's
//! queue label; a refused requeue counts nothing; a queue name the crate does not know is
//! counted under `other` and never becomes a label of its own. The counter is read from the
//! installed recorder's text exposition. The recorder is process-wide, so each test uses a queue
//! label of its own and compares before and after.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::jobs::{Settings, durable_store};
use spot_cli::services::ports::{JobAdmin, PortError, StopGate, StoreJobAdmin};
use spot_core::SiteId;
use spot_jobs::JobStore;
use spot_jobs::model::{InsertResult, JobId, JobState, NewJob};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

const T0: i64 = 1_795_262_400;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn site() -> SiteId {
    SiteId::new("site-1").unwrap()
}

/// The recorder's count for `spot_job_requeued_total{queue="<label>"}` (0 when the series does
/// not exist yet).
fn counted(label: &str) -> u64 {
    spot_telemetry::metrics::install().unwrap();
    let text = spot_telemetry::render_prometheus();
    let series = format!("spot_job_requeued_total{{queue=\"{label}\"}}");
    text.lines()
        .find_map(|line| line.strip_prefix(series.as_str()))
        .and_then(|rest| rest.trim().parse::<f64>().ok())
        .map_or(0, |value| value as u64)
}

/// Inserts a job on `queue`, claims it and completes it as dead.
async fn dead_job(store: &Arc<dyn JobStore>, queue: &str) -> JobId {
    let job = NewJob::new(
        "app:test".to_owned(),
        serde_json::json!({}),
        queue.to_owned(),
        site(),
        at(T0),
    );
    let InsertResult::Inserted(id) = store.insert_if_new(job).await.unwrap() else {
        panic!("not inserted");
    };
    let claimed = store
        .claim(
            &[queue.to_owned()],
            "worker-1",
            Duration::from_secs(30),
            at(T0),
            1,
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    store
        .complete(&claimed[0].claim, JobState::Dead, Some("boom".to_owned()))
        .await
        .unwrap();
    id
}

fn admin(store: &Arc<dyn JobStore>) -> StoreJobAdmin {
    StoreJobAdmin::new(site(), Arc::clone(store), StopGate::new())
}

#[tokio::test]
async fn a_requeue_through_the_port_counts_once_with_the_queue_label_and_a_refusal_counts_nothing()
{
    spot_telemetry::metrics::install().unwrap();
    spot_cli::services::telemetry::describe_all();
    let store = Settings::memory_store();
    let admin = admin(&store);
    let id = dead_job(&store, "short").await;
    let before = counted("short");

    admin.requeue(id, at(T0 + 10)).await.unwrap();
    assert_eq!(
        counted("short"),
        before + 1,
        "one successful requeue, one count"
    );

    // Refused: the job is pending now, so a second requeue changes nothing and counts nothing.
    let error = admin.requeue(id, at(T0 + 11)).await.unwrap_err();
    assert!(matches!(error, PortError::Invalid(_)), "{error:?}");
    // Refused: an id that does not exist.
    let error = admin
        .requeue(JobId(9_999_999), at(T0 + 11))
        .await
        .unwrap_err();
    assert!(matches!(error, PortError::Invalid(_)), "{error:?}");
    assert_eq!(
        counted("short"),
        before + 1,
        "a refused requeue is not counted"
    );

    // A second dead job counts again, on the same series.
    let again = dead_job(&store, "short").await;
    admin.requeue(again, at(T0 + 12)).await.unwrap();
    assert_eq!(counted("short"), before + 2);
}

#[tokio::test]
async fn an_unknown_queue_is_counted_under_other_and_never_as_its_own_series() {
    spot_telemetry::metrics::install().unwrap();
    let store = Settings::memory_store();
    let admin = admin(&store);
    let hostile = "tenant-acme-4711";
    let id = dead_job(&store, hostile).await;
    let before = counted("other");
    admin.requeue(id, at(T0 + 10)).await.unwrap();
    assert_eq!(counted("other"), before + 1);
    let text = spot_telemetry::render_prometheus();
    assert!(
        !text.contains(hostile),
        "a queue name never becomes a label"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgresql_store_the_host_builds_counts_a_port_requeue_too() {
    spot_telemetry::metrics::install().unwrap();
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    let mut cfg = db.config();
    cfg.application_name = "job_requeue_metric".to_owned();
    cfg.pool_max_size = 4;
    let backend = PgBackend::connect_unchecked(cfg).unwrap();
    let pg = durable_store(&backend);
    pg.prepare().await.unwrap();
    let store: Arc<dyn JobStore> = Arc::new(pg);
    let admin = admin(&store);
    let id = dead_job(&store, "long").await;
    let before = counted("long");

    admin.requeue(id, at(T0 + 10)).await.unwrap();
    assert_eq!(counted("long"), before + 1);
    assert!(admin.requeue(id, at(T0 + 11)).await.is_err());
    assert_eq!(
        counted("long"),
        before + 1,
        "a refused requeue is not counted"
    );
}
