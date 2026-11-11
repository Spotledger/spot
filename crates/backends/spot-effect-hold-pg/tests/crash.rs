//! The crash test of WP-MIG-08.T2: the ledger outlives the process that wrote it.
//!
//! A restart is simulated the way it happens: the pool is closed (every connection dies) and a
//! fresh pool is opened over the same database, exactly as a restarted process would. The hold
//! state, the effects with their ids, sequences, states and payload references, and the release
//! checkpoint must all come back identical — and a retried append must still be a no-op rather
//! than a second effect, or a crash would send a held mail twice.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_effect_hold::{
    EffectClass, EffectRef, EffectState, HoldState, LedgerStore, NewEffect, PayloadRef, Sequence,
};
use spot_effect_hold_pg::PgLedger;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_hold_the_effects_in_order_and_the_checkpoint() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let site = site("hold-restart.example");
    let migration = mig("mig-1");
    let progress = cursor(&migration, 1, Some(2));

    // Phase one: take the hold, hold three effects, dispatch one, record a checkpoint. Then every
    // connection closes: nothing is left in memory.
    let (ids, hold, before) = {
        let pool = pool(&db, 4, "restart-1");
        let ledger = PgLedger::new(pool.clone());
        ledger.prepare().await.unwrap();
        let hold = held(&migration, 10);
        assert!(
            ledger
                .compare_and_set_hold(&site, &HoldState::Open, hold.clone())
                .await
                .unwrap()
        );

        let mut ids = Vec::new();
        for (created_at, key) in [(1, "a"), (2, "b"), (3, "c")] {
            let new =
                if key == "b" {
                    // One effect with every optional field set, so the round trip is not just the
                    // easy case.
                    NewEffect {
                        effect: EffectRef::new(
                            site.clone(),
                            EffectClass::Webhook,
                            "invoice_webhook",
                            key,
                            created_at,
                        )
                        .with_source("outbox:invoices")
                        .with_source_event("evt-1")
                        .with_payload_ref(PayloadRef::stored("payload/handle-1", "sha256:abc", 42)),
                        migration_id: migration.clone(),
                    }
                } else {
                    effect(&site, key, created_at, &migration)
                };
            ids.push(ledger.append_effect(new).await.unwrap());
        }
        ledger
            .transition_effect(
                ids[1],
                EffectState::Held,
                EffectState::Dispatched,
                Some("sent".to_owned()),
            )
            .await
            .unwrap();
        ledger.checkpoint(&site, progress.clone()).await.unwrap();
        let before = ledger
            .list_effects(&site, &[EffectState::Held, EffectState::Dispatched])
            .await
            .unwrap();
        assert_eq!(keys(&before), ["a", "b", "c"]);
        pool.close();
        (ids, hold, before)
    };

    // Phase two: a fresh process, a fresh pool, the same database.
    let ledger = ledger(&db, 4, "restart-2").await;

    assert_eq!(
        ledger.get_hold(&site).await.unwrap(),
        hold,
        "the hold survived"
    );
    let after = ledger
        .list_effects(&site, &[EffectState::Held, EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(after, before, "every effect came back exactly, in order");
    assert_eq!(
        after.iter().map(|record| record.id).collect::<Vec<_>>(),
        ids
    );
    assert_eq!(
        after
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        [Sequence(1), Sequence(2), Sequence(3)]
    );
    assert_eq!(
        ledger.get_checkpoint(&site, &migration).await.unwrap(),
        Some(progress),
        "a release resumes from the checkpoint the dead process wrote"
    );

    // The retried append that a crashed dispatcher makes is still a deduplication, not a second
    // effect: same id, same sequence, same state, same note.
    let retried = ledger
        .append_effect(effect(&site, "b", 99, &migration))
        .await
        .unwrap();
    assert_eq!(retried, ids[1]);
    let dispatched = ledger
        .list_effects(&site, &[EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].sequence, Sequence(2));
    assert_eq!(dispatched[0].note.as_deref(), Some("sent"));

    // A new append continues after the highest sequence rather than reusing a number.
    ledger
        .append_effect(effect(&site, "d", 4, &migration))
        .await
        .unwrap();
    let all = ledger
        .list_effects(&site, &[EffectState::Held, EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(keys(&all), ["a", "b", "c", "d"]);
    assert_eq!(all[3].sequence, Sequence(4));
}

/// A released hold also survives: the state is stored, not recomputed from the in-memory gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_hold_is_still_released_after_a_restart() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let site = site("hold-restart-released.example");
    let migration = mig("mig-1");
    let released = HoldState::Released {
        migration_id: migration.clone(),
        at: 20,
    };

    {
        let pool = pool(&db, 2, "released-1");
        let ledger = PgLedger::new(pool.clone());
        ledger.prepare().await.unwrap();
        assert!(
            ledger
                .compare_and_set_hold(&site, &HoldState::Open, held(&migration, 10))
                .await
                .unwrap()
        );
        assert!(
            ledger
                .compare_and_set_hold(&site, &held(&migration, 10), released.clone())
                .await
                .unwrap()
        );
        pool.close();
    }

    let ledger = ledger(&db, 2, "released-2").await;
    assert_eq!(ledger.get_hold(&site).await.unwrap(), released);
    // A released site remembers its migration; a `begin` may move it to Held again.
    assert!(
        ledger
            .compare_and_set_hold(&site, &released, held(&mig("mig-2"), 30))
            .await
            .unwrap()
    );
    assert!(ledger.get_hold(&site).await.unwrap().is_held());
}
