//! Conditions hosted against two real nodes (WP-RTB-01.T2).
//!
//! No detector exists yet to raise a real condition over the wire (out of scope for this task),
//! and this service exposes nothing on HTTP (ADR-067 section 1's "in-process callers only"), so
//! `Cluster`/`Node` (which drive a node only through its HTTP API) cannot exercise it. The brief
//! allows exactly this: "restrict this case to the host-level API the handle exposes via the
//! test crate". Each "node" here is therefore its own `spot_cli::services::conditions::build`
//! instance, driven directly through [`spot_cli::services::Service`] and
//! [`spot_cli::services::conditions::ConditionsHandle`] — the same host-level construction
//! `spot-cli`'s own `tests/conditions_host.rs` proves, now proved to be correctly isolated node
//! from node, each over its own real PostgreSQL database (`spot_postgres::testing::TestDb`, the
//! same scratch-database fixture `Node::create` itself uses).
//!
//! This crate owns no dependency of its own on `spot-conditions`, `spot-conditions-pg` or
//! `spot-storage-contract`: every type and trait this file needs reaches it through
//! `spot_cli::services::conditions`'s re-exports (see that module's docs), so nothing here
//! required a `Cargo.toml` change outside this one file.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails, as the sibling `*-pg` crates do.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::Service;
use spot_cli::services::conditions::{
    Catalog, ChangeFeed, Clock, Collation, ConditionParams, ConditionRegistrations,
    ConditionsHandle, FeedPosition, ManualClock, Millis, OffsetDateTime, PgConditionStore,
    RaiseOutcome, Settings, Severity, StorageBackend, build,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use tokio::sync::watch;

/// One of the "two real nodes" the brief asks for: its own database, its own conditions engine
/// task, built and checked exactly as `spot_cli::host::start_with` would build the module's
/// service for a real site, just without the rest of the site around it (metadata, auth,
/// routes) that this test does not need.
struct TestNode {
    _db: TestDb,
    backend: PgBackend,
    clock: Arc<ManualClock>,
    handle: ConditionsHandle,
    stop: watch::Sender<bool>,
}

fn fixed_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(),
    ))
}

fn now_millis(clock: &ManualClock) -> Millis {
    let micros = clock.now_utc().unix_timestamp_nanos() / 1_000_000;
    Millis::new(u64::try_from(micros.max(0)).unwrap())
}

fn fast_settings() -> Settings {
    Settings {
        tick_seconds: 1,
        amber_needs_person_after_seconds: 300,
    }
}

async fn node() -> Option<TestNode> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    let clock = fixed_clock();
    let store = Arc::new(PgConditionStore::new(backend.clone(), catalog));
    // The host never creates a table (`spot migrate` does); this test database gets them here.
    store.prepare().await.unwrap();
    let (service, handle) = build(
        "node".to_owned(),
        store,
        Arc::clone(&clock) as Arc<dyn Clock>,
        fast_settings(),
        Arc::new(ConditionRegistrations::default()),
    );
    service.check().await.unwrap();
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = service.run(stop_rx).await;
    });
    Some(TestNode {
        _db: db,
        backend,
        clock,
        handle,
        stop,
    })
}

async fn wait_for_a_tick() {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
}

#[tokio::test]
async fn a_condition_raised_on_one_node_is_never_visible_on_the_other() {
    let Some(a) = node().await else { return };
    let Some(b) = node().await else { return };

    a.handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&a.clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;

    assert_eq!(a.handle.open().await.unwrap().len(), 1);
    assert!(b.handle.open().await.unwrap().is_empty());

    let _ = a.stop.send(true);
    let _ = b.stop.send(true);
}

#[tokio::test]
async fn a_raise_and_a_clear_on_one_node_write_the_matching_outbox_events_and_only_there() {
    let Some(a) = node().await else { return };
    let Some(b) = node().await else { return };

    a.handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&a.clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    a.clock.advance(Duration::from_secs(65));
    a.handle
        .observe_cleared("NODE_SILENT", "branch-12", now_millis(&a.clock))
        .await
        .unwrap()
        .unwrap();
    // The engine's debounce window (60s) must close before the clear commits.
    a.clock.advance(Duration::from_secs(65));
    wait_for_a_tick().await;

    let a_events: Vec<String> = a
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .map(|records| {
            records
                .into_iter()
                .map(|r| r.event.topic.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(a_events.contains(&"sys_condition_raised".to_owned()));
    assert!(a_events.contains(&"sys_condition_cleared".to_owned()));
    assert!(a.handle.open().await.unwrap().is_empty());

    let b_events = b
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert!(b_events.is_empty(), "node b must never see node a's events");

    let _ = a.stop.send(true);
    let _ = b.stop.send(true);
}

#[tokio::test]
async fn a_condition_on_one_node_survives_that_nodes_own_restart() {
    let Some(a) = node().await else { return };
    let Some(b) = node().await else { return };

    a.handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&a.clock),
        )
        .await
        .unwrap()
        .unwrap();
    wait_for_a_tick().await;
    let _ = a.stop.send(true);

    // "Restart" node `a` over the same database; node `b` never stops and never sees it.
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    let store = Arc::new(PgConditionStore::new(a.backend.clone(), catalog));
    let clock = fixed_clock();
    let (service, handle) = build(
        "node".to_owned(),
        store,
        clock as Arc<dyn Clock>,
        fast_settings(),
        Arc::new(ConditionRegistrations::default()),
    );
    service.check().await.unwrap();
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = service.run(stop_rx).await;
    });

    let open = handle.open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), "NODE_SILENT");
    assert!(b.handle.open().await.unwrap().is_empty());

    let outcome = a
        .handle
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            now_millis(&a.clock),
        )
        .await;
    // The first `a.handle` is now talking to nobody: the task behind it stopped.
    assert!(outcome.is_err());

    let _ = stop.send(true);
    let _ = b.stop.send(true);
}

#[test]
fn raise_outcomes_and_severities_are_reachable_through_the_re_export() {
    // Proves the host-level API this file depends on (no direct dependency on
    // `spot-conditions`) actually names the types the brief's handle signature promises.
    let _: RaiseOutcome = RaiseOutcome::Raised;
    let _: Severity = Severity::Amber;
}
