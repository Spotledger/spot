//! The durability gate over a real PostgreSQL outbox (WP-RTB-01.T4): a write whose handler
//! enqueues an outbox event is acknowledged only once a journal that becomes durable later has
//! reached the head read after the commit; under halt a request is refused before its handler
//! runs; an unconfirmed commit answers `journal.commit_outcome_unknown`.
//!
//! The journal is a fake whose durable position the test moves, so the timing is exact; the real
//! shipper behind the real gate is `l3-journal-sync`'s `durability.rs`. Skips cleanly (printing
//! `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::post;
use http_body_util::BodyExt;
use spot_durability_gate::{
    GateConfig, GateFuture, GateState, HeadSource, Journal, Kind, OnUnreachable, Policy,
    PolicyCell, Stats, gate,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, ChangeFeed, Collation, FeedPosition, Isolation, Name, NodeRef, Outbox, OutboxEvent,
    RecordId, StorageBackend, Transaction, TxOptions,
};
use tower::ServiceExt;

const HEAD_SQL: &str = "SELECT (SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'), \
                        last_seq FROM spot_feed_state WHERE log = 'outbox'";

struct PgHead(PgBackend);

impl HeadSource for PgHead {
    fn head(&self) -> GateFuture<'_, Option<(u64, u64)>> {
        Box::pin(async move {
            let _ = self.0.read_outbox(FeedPosition::START, 1).await;
            let conn = self.0.pool().get().await.ok()?;
            let row = conn.query_one(HEAD_SQL, &[]).await.ok()?;
            let generation: i64 = row.try_get(0).ok()?;
            let seq: i64 = row.try_get(1).ok()?;
            Some((u64::try_from(generation).ok()?, u64::try_from(seq).ok()?))
        })
    }
}

/// A journal whose durable position the test moves.
struct TestJournal {
    durable: AtomicU64,
    unreachable: AtomicBool,
}

impl Journal for TestJournal {
    fn durable(&self) -> (u64, u64) {
        (1, self.durable.load(Ordering::SeqCst))
    }

    fn wait_durable(&self, position: (u64, u64), timeout: Duration) -> GateFuture<'_, bool> {
        Box::pin(async move {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if self.durable.load(Ordering::SeqCst) >= position.1 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        })
    }

    fn unreachable(&self) -> bool {
        self.unreachable.load(Ordering::SeqCst)
    }
}

struct App {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    runs: AtomicU64,
}

async fn write(State(app): State<Arc<App>>) -> &'static str {
    app.runs.fetch_add(1, Ordering::SeqCst);
    let mut options = TxOptions::new(
        Name::new("durability_test").unwrap(),
        Isolation::ReadCommitted,
    );
    options.max_writes = 10;
    let mut tx = app
        .backend
        .begin(Arc::clone(&app.catalog), options)
        .await
        .unwrap();
    let payload = format!(
        "{{\"meta\":{{\"occurred_at\":\"2026-01-01T00:00:00Z\",\"actor\":{{\"kind\":\"system\",\
         \"component\":\"scheduler\"}},\"correlation_id\":\"01900000-0000-7000-8000-{:012x}\"}},\
         \"data\":{{}}}}",
        app.runs.load(Ordering::SeqCst)
    );
    tx.enqueue(&OutboxEvent {
        id: RecordId::new_v7(),
        topic: Name::new("sys_condition_raised").unwrap(),
        subject: NodeRef {
            entity: Name::new("condition").unwrap(),
            id: RecordId::new_v7(),
        },
        subject_version: None,
        schema_version: 1,
        payload,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    "written"
}

async fn rig(
    on: OnUnreachable,
) -> Option<(
    tower::util::BoxCloneService<Request<Body>, axum::response::Response, std::convert::Infallible>,
    Arc<App>,
    Arc<TestJournal>,
    Arc<Stats>,
    TestDb,
)> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    let app = Arc::new(App {
        backend: backend.clone(),
        catalog,
        runs: AtomicU64::new(0),
    });
    let journal = Arc::new(TestJournal {
        durable: AtomicU64::new(0),
        unreachable: AtomicBool::new(false),
    });
    let stats = Arc::new(Stats::default());
    let state = Arc::new(GateState {
        policy: Arc::new(PolicyCell::new(Policy {
            kind: Kind::RemoteSync,
            on_unreachable: on,
            journal_on: true,
        })),
        head: Arc::new(PgHead(backend)),
        journal: journal.clone(),
        standby: None,
        config: GateConfig {
            wait: Duration::from_millis(400),
            retry_after_secs: 3,
        },
        stats: Arc::clone(&stats),
    });
    let router = Router::new()
        .route("/w", post(write))
        .with_state(Arc::clone(&app))
        .route_layer(from_fn_with_state(state, gate));
    Some((
        tower::util::BoxCloneService::new(router.into_service()),
        app,
        journal,
        stats,
        db,
    ))
}

async fn post_w(
    service: &tower::util::BoxCloneService<
        Request<Body>,
        axum::response::Response,
        std::convert::Infallible,
    >,
) -> (StatusCode, String) {
    let response = service
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/w")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_is_acknowledged_only_once_the_journal_reaches_the_head_after_its_commit() {
    let Some((service, app, journal, stats, _db)) = rig(OnUnreachable::Degrade).await else {
        return;
    };
    let mover = Arc::clone(&journal);
    // The journal catches up 120 ms after the request starts.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        mover.durable.store(1, Ordering::SeqCst);
    });
    let started = Instant::now();
    let (status, body) = post_w(&service).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "written"));
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the response waited for the chunk"
    );
    assert_eq!(app.runs.load(Ordering::SeqCst), 1);
    assert!(stats.p99_millis() >= 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_halt_a_refusal_runs_no_handler_and_an_unconfirmed_commit_says_so() {
    let Some((service, app, journal, _stats, _db)) = rig(OnUnreachable::Halt).await else {
        return;
    };
    journal.unreachable.store(true, Ordering::SeqCst);
    let (status, body) = post_w(&service).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("journal.halted"));
    assert_eq!(
        app.runs.load(Ordering::SeqCst),
        0,
        "no engine operation ran"
    );
    // The store looks reachable but never confirms: the write is committed, the answer unknown.
    journal.unreachable.store(false, Ordering::SeqCst);
    let (status, body) = post_w(&service).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("journal.commit_outcome_unknown"));
    assert_eq!(app.runs.load(Ordering::SeqCst), 1);
    // The journal catches up: the next write is acknowledged.
    journal.durable.store(100, Ordering::SeqCst);
    let (status, _) = post_w(&service).await;
    assert_eq!(status, StatusCode::OK);
}
