//! The ledger's edge rules: deduplication changes nothing, compare-and-set never invents a state,
//! listing is tenant-scoped and honours an empty filter, and a checkpoint is replaced, not
//! appended to.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_effect_hold::{
    EffectClass, EffectRef, EffectState, Error, HoldState, LedgerStore, NewEffect, PayloadRef,
    Sequence,
};
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_append_returns_the_original_row_unchanged() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 4, "hold_semantics").await;
    let site = site("hold-duplicate.example");
    let migration = mig("mig-1");

    let id = ledger
        .append_effect(effect(&site, "k1", 1, &migration))
        .await
        .unwrap();
    ledger
        .transition_effect(
            id,
            EffectState::Held,
            EffectState::Dispatched,
            Some("sent".to_owned()),
        )
        .await
        .unwrap();

    // A retry that says something different about the same key still changes nothing: the row
    // keeps its id, sequence, class, payload and state.
    let different = NewEffect {
        effect: EffectRef::new(
            site.clone(),
            EffectClass::Webhook,
            "invoice_webhook",
            "k1",
            99,
        )
        .with_payload_ref(PayloadRef::stored("payload/other", "sha256:other", 7)),
        migration_id: migration.clone(),
    };
    assert_eq!(
        ledger.append_effect(different).await.unwrap(),
        id,
        "a duplicate returns the stored id"
    );

    let dispatched = ledger
        .list_effects(&site, &[EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(dispatched.len(), 1);
    let stored = &dispatched[0];
    assert_eq!(stored.id, id);
    assert_eq!(stored.sequence, Sequence(1));
    assert_eq!(stored.class, EffectClass::Mail);
    assert_eq!(stored.payload_ref, PayloadRef::Absent);
    assert_eq!(stored.created_at, 1);
    assert_eq!(stored.note.as_deref(), Some("sent"));
    assert!(
        ledger
            .list_effects(&site, &[EffectState::Held])
            .await
            .unwrap()
            .is_empty(),
        "the duplicate did not add a held effect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compare_and_set_from_a_state_that_was_never_stored_fails() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 2, "hold_semantics_cas").await;
    let site = site("hold-cas-unstored.example");
    let migration = mig("mig-1");

    // The site has never been held, so it is `Open`, not `Held`. A `Held -> Released` swap must
    // not create the row it claims to match.
    assert!(
        !ledger
            .compare_and_set_hold(
                &site,
                &held(&migration, 1),
                HoldState::Released {
                    migration_id: migration.clone(),
                    at: 2,
                },
            )
            .await
            .unwrap()
    );
    assert_eq!(ledger.get_hold(&site).await.unwrap(), HoldState::Open);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_is_tenant_scoped_and_an_empty_filter_lists_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 4, "hold_semantics_list").await;
    let mine = site("hold-list-a.example");
    let other = site("hold-list-b.example");
    let migration = mig("mig-1");

    ledger
        .append_effect(effect(&mine, "mine", 1, &migration))
        .await
        .unwrap();
    ledger
        .append_effect(effect(&other, "other", 1, &migration))
        .await
        .unwrap();

    let listed = ledger
        .list_effects(&mine, &[EffectState::Held])
        .await
        .unwrap();
    assert_eq!(keys(&listed), ["mine"]);
    assert!(
        ledger.list_effects(&mine, &[]).await.unwrap().is_empty(),
        "an empty state filter asks for no states, not for all of them"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_effect_id_is_invalid_input_not_a_silent_no_op() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 2, "hold_semantics_state").await;
    let error = ledger
        .transition_effect(
            spot_effect_hold::EffectId(1),
            EffectState::Held,
            EffectState::Discarded,
            Some("rolled back".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::InvalidInput { .. }),
        "expected invalid input, got {error}"
    );
    assert_eq!(error.code(), spot_effect_hold::codes::INVALID_INPUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_is_replaced_and_an_absent_one_is_none() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 2, "hold_semantics_checkpoint").await;
    let site = site("hold-checkpoint-upsert.example");
    let migration = mig("mig-1");

    assert_eq!(
        ledger.get_checkpoint(&site, &migration).await.unwrap(),
        None,
        "a migration that never released has no checkpoint"
    );
    ledger
        .checkpoint(&site, cursor(&migration, 1, Some(1)))
        .await
        .unwrap();
    ledger
        .checkpoint(&site, cursor(&migration, 2, Some(2)))
        .await
        .unwrap();
    assert_eq!(
        ledger.get_checkpoint(&site, &migration).await.unwrap(),
        Some(cursor(&migration, 2, Some(2))),
        "the second checkpoint replaced the first"
    );
    // Another migration of the same site keeps its own checkpoint.
    let other = mig("mig-2");
    assert_eq!(ledger.get_checkpoint(&site, &other).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 2, "hold_semantics_prepare").await;
    // The helper prepared once; preparing again is a no-op, as a restart of every node needs.
    ledger.prepare().await.unwrap();
    ledger.prepare().await.unwrap();
    assert_eq!(
        ledger
            .get_hold(&site("hold-prepare.example"))
            .await
            .unwrap(),
        HoldState::Open
    );
}
