//! The ledger under the real hold service: `begin`, held effects, `release` in the original order,
//! and the release checkpoint a crash resumes from.
//!
//! The contract suite proves the ledger's own operations. This file proves the service the platform
//! actually runs drives it correctly: effects leave in their sequence order, every dispatch moves
//! the checkpoint forward, and a release that starts after a crash resumes at the checkpoint
//! instead of re-sending what was already dispatched.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_effect_hold::{
    EffectState, HoldService, HoldState, LedgerStore, PolicyTable, RecordingAudit,
    RecordingDispatcher, RecordingNotifier, ReleasePlan, Sequence,
};
use spot_effect_hold_pg::PgLedger;
use spot_postgres::testing::TestDb;

/// A service over `ledger` and a recording dispatcher, plus that dispatcher.
fn service(
    ledger: Arc<dyn LedgerStore>,
    site: spot_core::SiteId,
) -> (HoldService, Arc<RecordingDispatcher>) {
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let service = HoldService::new(
        site,
        ledger,
        Arc::new(PolicyTable::new()),
        Arc::new(RecordingAudit::new()),
        dispatcher.clone(),
        Arc::new(RecordingNotifier::new()),
    );
    (service, dispatcher)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_dispatches_in_the_original_order_and_checkpoints_every_effect() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let site = site("hold-release.example");
    let migration = mig("mig-1");
    let ledger: Arc<dyn LedgerStore> = Arc::new(ledger(&db, 4, "hold_release").await);
    let (service, dispatcher) = service(Arc::clone(&ledger), site.clone());

    service.begin("mig-1", "verify", 10, None).await.unwrap();
    for key in ["a", "b", "c"] {
        ledger
            .append_effect(effect(&site, key, 1, &migration))
            .await
            .unwrap();
    }

    let report = service
        .release("mig-1", ReleasePlan::new(), 20)
        .await
        .unwrap();
    assert!(report.completed, "the release finished");
    assert_eq!(
        dispatcher
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"],
        "effects leave in their original order"
    );

    let cursor = ledger
        .get_checkpoint(&site, &migration)
        .await
        .unwrap()
        .expect("the release wrote a checkpoint");
    assert_eq!(cursor.dispatched, 3);
    assert_eq!(cursor.last_sequence, Some(Sequence(3)));
    assert_eq!(
        ledger.get_hold(&site).await.unwrap(),
        HoldState::Released {
            migration_id: migration.clone(),
            at: 20,
        }
    );
    let dispatched = ledger
        .list_effects(&site, &[EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(keys(&dispatched), ["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_after_a_crash_resumes_from_the_checkpoint_and_repeats_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let site = site("hold-release-crash.example");
    let migration = mig("mig-1");
    let progress = cursor(&migration, 1, Some(1));

    // Phase one: begin, hold three effects, and get as far as dispatching the first one before
    // every connection dies mid-release.
    {
        let pool = pool(&db, 4, "release-crash-1");
        let prepared = PgLedger::new(pool.clone());
        prepared.prepare().await.unwrap();
        let ledger: Arc<dyn LedgerStore> = Arc::new(prepared);
        let (service, _) = service(Arc::clone(&ledger), site.clone());
        service.begin("mig-1", "verify", 10, None).await.unwrap();
        let mut ids = Vec::new();
        for (created_at, key) in [(1, "a"), (2, "b"), (3, "c")] {
            ids.push(
                ledger
                    .append_effect(effect(&site, key, created_at, &migration))
                    .await
                    .unwrap(),
            );
        }
        // What a release that crashed after its first effect leaves behind.
        ledger
            .transition_effect(ids[0], EffectState::Held, EffectState::Dispatched, None)
            .await
            .unwrap();
        ledger.checkpoint(&site, progress.clone()).await.unwrap();
        pool.close();
    }

    // Phase two: a restarted process releases again.
    let ledger: Arc<dyn LedgerStore> = Arc::new(ledger(&db, 4, "release-crash-2").await);
    let (service, dispatcher) = service(Arc::clone(&ledger), site.clone());
    let report = service
        .release("mig-1", ReleasePlan::new(), 20)
        .await
        .unwrap();

    assert_eq!(
        report.resumed_from,
        Some(progress),
        "the release resumed from the checkpoint the crashed process wrote"
    );
    assert_eq!(
        dispatcher
            .calls()
            .iter()
            .map(|call| call.key.as_str())
            .collect::<Vec<_>>(),
        ["b", "c"],
        "the already-dispatched effect was not sent again"
    );
    let cursor = ledger
        .get_checkpoint(&site, &migration)
        .await
        .unwrap()
        .expect("the resumed release wrote a checkpoint");
    assert_eq!(cursor.dispatched, 3, "the checkpoint counts both runs");
    assert_eq!(cursor.last_sequence, Some(Sequence(3)));
    assert_eq!(
        ledger.get_hold(&site).await.unwrap(),
        HoldState::Released {
            migration_id: migration,
            at: 20,
        }
    );
}
