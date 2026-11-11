//! The conditions service hosted directly (no `SiteCtx`, the same plain [`Service`] trait a
//! reload or the registry itself drives) against a real PostgreSQL database.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails, as the sibling `*-pg` crates do.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::Service;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_cli::services::conditions::{
    ConditionRegistrations, ConditionStore, ConditionsHandle, Settings, StoreFuture, build,
};
use spot_conditions::{AuditEvent, ConditionParams, Millis, Open};
use spot_conditions_pg::{ConditionsPgError, PgConditionStore};
use spot_core::{Clock, ManualClock};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{Catalog, ChangeFeed, Collation, StorageBackend};
use time::macros::datetime;
use tokio::sync::watch;

struct Env {
    _db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    // The host never creates a table (`spot migrate` does, from `schema::extra_tables`); the
    // test database gets them the way the store crate's own tests do.
    PgConditionStore::new(backend.clone(), Arc::clone(&catalog))
        .prepare()
        .await
        .unwrap();
    Some(Env {
        _db: db,
        backend,
        catalog,
    })
}

fn fast_settings() -> Settings {
    Settings {
        tick_seconds: 1,
        amber_needs_person_after_seconds: 300,
    }
}

/// A fresh manual clock fixed at the same instant every call, so two service instances built
/// one after the other (a "restart" in these tests) start from the same `now` unless a test
/// explicitly advances one of them.
fn fixed_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-01-01 00:00:00 UTC)))
}

/// The engine instant a [`ConditionsService`] actor would compute from `clock`, in exactly the
/// same conversion ([`spot_cli::services::conditions`]'s `Actor::now`): milliseconds since the
/// Unix epoch. Callers of [`ConditionsHandle::raise`] and
/// [`ConditionsHandle::observe_cleared`] must pass an instant on this same timeline, because the
/// engine compares it against the tick's own clock-derived instant when it escalates or decides
/// what needs a person; a caller on a different timeline (for example milliseconds since its own
/// local zero) would make every elapsed-time comparison meaningless.
fn now_millis(clock: &ManualClock) -> Millis {
    let micros = clock.now_utc().unix_timestamp_nanos() / 1_000_000;
    Millis::new(u64::try_from(micros.max(0)).unwrap())
}

/// Builds, checks and starts a service over `env`'s database with its own manual clock,
/// returning the handle, a stop signal the caller must send before the test ends, and the clock
/// (so the test can read or advance the same instant the running actor uses).
///
/// The task is otherwise leaked for the test's life, which is fine for a short-lived test
/// process but is not the pattern a real host uses: `Services` does this through its own
/// start/shutdown (see `services/mod.rs`).
async fn start(
    env: &Env,
    settings: Settings,
    clock: Arc<ManualClock>,
) -> (ConditionsHandle, watch::Sender<bool>) {
    let store = Arc::new(PgConditionStore::new(
        env.backend.clone(),
        Arc::clone(&env.catalog),
    ));
    start_with(store, settings, clock).await
}

/// A real store whose writes fail while `down` is set: the outage is injected at the store seam,
/// with no SQL text, and the very same store (and backend) serves again once it is cleared.
struct Flaky {
    inner: PgConditionStore,
    down: Arc<AtomicBool>,
}

impl ConditionStore for Flaky {
    fn load_open(&self) -> StoreFuture<'_, Vec<Open>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ConditionsPgError::Unavailable);
            }
            self.inner.load_open().await
        })
    }

    fn apply<'a>(&'a self, events: &'a [AuditEvent]) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ConditionsPgError::Unavailable);
            }
            self.inner.apply(events).await
        })
    }
}

async fn start_with(
    store: Arc<dyn ConditionStore>,
    settings: Settings,
    clock: Arc<ManualClock>,
) -> (ConditionsHandle, watch::Sender<bool>) {
    let (service, handle) = build(
        "acme".to_owned(),
        store,
        clock as Arc<dyn Clock>,
        settings,
        Arc::new(ConditionRegistrations::default()),
    );
    service.check().await.unwrap();
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = service.run(stop_rx).await;
    });
    (handle, stop_tx)
}

async fn wait_for_a_tick() {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
}

#[tokio::test]
async fn a_raise_is_still_open_after_the_host_restarts() {
    let Some(env) = env().await else { return };
    let clock = fixed_clock();
    let (handle, stop) = start(&env, fast_settings(), Arc::clone(&clock)).await;
    handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new().with("silent_hours", "2"),
            now_millis(&clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    let _ = stop.send(true);

    // "Restart": a brand new service built over the same database.
    let (handle, stop) = start(&env, fast_settings(), fixed_clock()).await;
    let open = handle.open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), "NODE_SILENT");
    assert_eq!(open[0].subject.as_str(), "branch-12");
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_restarted_condition_escalates_on_its_original_schedule() {
    let Some(env) = env().await else { return };
    let clock = fixed_clock();
    let (handle, stop) = start(&env, fast_settings(), Arc::clone(&clock)).await;
    handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    let _ = stop.send(true);

    // The "restart": a fresh clock at the same starting instant as the one the condition was
    // raised on, then advanced past NODE_SILENT's catalogue schedule (red at 48 hours open).
    let clock = fixed_clock();
    let (handle, stop) = start(&env, fast_settings(), Arc::clone(&clock)).await;
    clock.advance(Duration::from_secs(48 * 3_600));
    wait_for_a_tick().await;
    let open = handle.open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].severity, spot_conditions::Severity::Red);
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_clear_writes_sys_condition_cleared_and_opens_nothing_after_a_restart() {
    let Some(env) = env().await else { return };
    let clock = fixed_clock();
    let (handle, stop) = start(&env, fast_settings(), Arc::clone(&clock)).await;
    handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    clock.advance(Duration::from_secs(70));
    handle
        .observe_cleared("NODE_SILENT", "branch-12", now_millis(&clock))
        .await
        .unwrap()
        .unwrap();
    // The engine's debounce window (60s) must close before the clear commits.
    clock.advance(Duration::from_secs(65));
    wait_for_a_tick().await;
    let _ = stop.send(true);

    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.event.topic.as_str() == "sys_condition_cleared")
    );

    let (handle, stop) = start(&env, fast_settings(), fixed_clock()).await;
    assert!(handle.open().await.unwrap().is_empty());
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_raise_works_from_memory_while_the_store_is_down_and_lands_once_it_returns() {
    let Some(env) = env().await else { return };
    let clock = fixed_clock();
    let down = Arc::new(AtomicBool::new(false));
    let store = Arc::new(Flaky {
        inner: PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog)),
        down: Arc::clone(&down),
    });
    let (handle, stop) = start_with(store, fast_settings(), Arc::clone(&clock)).await;

    // The store goes down: every write fails until `down` is cleared.
    down.store(true, Ordering::SeqCst);
    for subject in ["branch-1", "branch-2", "branch-3"] {
        let outcome = handle
            .raise(
                "NODE_SILENT",
                subject,
                ConditionParams::new(),
                now_millis(&clock),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, spot_conditions::RaiseOutcome::Raised);
    }
    // In memory at once, whatever the store is doing.
    assert_eq!(handle.open().await.unwrap().len(), 3);
    wait_for_a_tick().await;
    let degraded = handle.readiness_text().await.unwrap();
    assert!(
        degraded.starts_with("degraded:"),
        "the store is down: the service must say so ({degraded})"
    );

    // Nothing landed: the real store sees no open row and no outbox event.
    let verify = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    assert!(verify.load_open().await.unwrap().is_empty());
    assert!(
        env.backend
            .read_outbox(FeedPosition::START, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // The store returns: the three transitions land, in the order they happened.
    down.store(false, Ordering::SeqCst);
    wait_for_a_tick().await;
    wait_for_a_tick().await;

    let text = handle.readiness_text().await.unwrap();
    assert!(text.starts_with("conditions:"), "{text}");
    assert_eq!(verify.load_open().await.unwrap().len(), 3);
    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    let subjects: Vec<String> = records
        .iter()
        .filter(|r| r.event.topic.as_str() == "sys_condition_raised")
        .map(|r| r.event.payload.clone())
        .collect();
    assert_eq!(subjects.len(), 3);
    for (payload, expected) in subjects.iter().zip(["branch-1", "branch-2", "branch-3"]) {
        assert!(payload.contains(expected), "out of order: {payload}");
    }
    let _ = stop.send(true);
}

#[tokio::test]
async fn two_sites_never_see_each_others_conditions() {
    let Some(a) = env().await else { return };
    let Some(b) = env().await else { return };
    let clock = fixed_clock();
    let (handle_a, stop_a) = start(&a, fast_settings(), Arc::clone(&clock)).await;
    let (handle_b, stop_b) = start(&b, fast_settings(), fixed_clock()).await;
    handle_a
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    assert_eq!(handle_a.open().await.unwrap().len(), 1);
    assert!(handle_b.open().await.unwrap().is_empty());
    let _ = stop_a.send(true);
    let _ = stop_b.send(true);
}
