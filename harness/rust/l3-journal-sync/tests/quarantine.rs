//! `QUARANTINED` raised and cleared through the real conditions service on two real nodes
//! (WP-RTB-03.T9): a held quarantine item raises the Red condition on its own node only, and the
//! condition clears when the item leaves the held state.
//!
//! Like `conditions.rs`, each "node" is its own `spot_cli::services::conditions::build` instance
//! over its own scratch PostgreSQL database; every type needed reaches this file through
//! `spot_cli::services::{conditions, quarantine}`. Skips (printing `SKIPPED:`) without
//! `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::Service;
use spot_cli::services::conditions::{
    Catalog, Clock, Collation, ConditionRegistrations, ConditionsHandle, ManualClock,
    OffsetDateTime, PgConditionStore, Settings, build,
};
use spot_cli::services::quarantine::condition::{HandleConditions, QuarantinedSync};
use spot_cli::services::quarantine::{Item, State};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::StorageBackend as _;
use tokio::sync::watch;

struct TestNode {
    _db: TestDb,
    clock: Arc<ManualClock>,
    handle: ConditionsHandle,
    stop: watch::Sender<bool>,
}

async fn node() -> Option<TestNode> {
    let Some(db) = TestDb::create().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(),
    ));
    let store = Arc::new(PgConditionStore::new(backend, catalog));
    store.prepare().await.unwrap();
    let (service, handle) = build(
        "node".to_owned(),
        store,
        Arc::clone(&clock) as Arc<dyn Clock>,
        Settings {
            tick_seconds: 1,
            amber_needs_person_after_seconds: 300,
        },
        Arc::new(ConditionRegistrations::default()),
    );
    service.check().await.unwrap();
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = service.run(stop_rx).await;
    });
    Some(TestNode {
        _db: db,
        clock,
        handle,
        stop,
    })
}

async fn wait_for_a_tick() {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
}

fn held(id: &str) -> Item {
    Item {
        id: id.to_owned(),
        doctype: "invoice".to_owned(),
        reason: "replica_apply.invalid".to_owned(),
        node: "branch-12".to_owned(),
        record: format!("invoice:{id}"),
        state: State::Held,
        age_secs: 60,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_item_raises_quarantined_on_its_own_node_only_and_it_clears() {
    let Some(a) = node().await else { return };
    let Some(b) = node().await else { return };
    let port = HandleConditions::new(a.handle.clone(), Arc::clone(&a.clock) as Arc<dyn Clock>);
    let sync = QuarantinedSync::new();

    let mut items = vec![held("1"), held("2")];
    let raised = sync.reconcile(&items, &port).await;
    assert_eq!(raised.raised.len(), 2, "{raised:?}");
    wait_for_a_tick().await;
    let open = a.handle.open().await.unwrap();
    assert_eq!(open.len(), 2);
    assert!(b.handle.open().await.unwrap().is_empty());

    // Item 1 is discarded: its condition clears once the engine's debounce window has closed.
    items[0].state = State::Discarded;
    a.clock.advance(Duration::from_secs(65));
    let cleared = sync.reconcile(&items, &port).await;
    assert_eq!(cleared.cleared, vec!["1".to_owned()], "{cleared:?}");
    a.clock.advance(Duration::from_secs(65));
    wait_for_a_tick().await;
    assert_eq!(a.handle.open().await.unwrap().len(), 1);

    let _ = a.stop.send(true);
    let _ = b.stop.send(true);
}
