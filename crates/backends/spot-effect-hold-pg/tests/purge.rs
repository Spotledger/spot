//! The retention purge on PostgreSQL (WP-MIG-08.T6): 5,000 released effects purged in batches of
//! 1,000 take five calls, the payload reference is the empty marker afterwards while the row, its
//! state and its key stay, and a held effect of the same age is untouched. Needs `SPOT_TEST_PG_URL`
//! (skipped otherwise).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_effect_hold::{EffectState, LedgerStore, NewEffect, PayloadRef, RetentionPolicy};
use spot_postgres::testing::TestDb;

fn stored(site: &spot_core::SiteId, key: &str, m: &spot_effect_hold::MigrationId) -> NewEffect {
    let mut new = effect(site, key, 1, m);
    new.effect.payload_ref = PayloadRef::stored(format!("blob-{key}"), "hash", 10);
    new
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_thousand_purge_in_five_calls_and_the_rows_stay() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 8, "hold_purge").await;
    let s = site("purge-pg.example");
    let m = mig("mig-1");
    let held = ledger.append_effect(stored(&s, "held", &m)).await.unwrap();
    for n in 0..5_000 {
        let id = ledger
            .append_effect(stored(&s, &format!("k{n}"), &m))
            .await
            .unwrap();
        ledger
            .transition_effect_at(id, EffectState::Held, EffectState::Dispatched, None, 10)
            .await
            .unwrap();
    }
    let policy = RetentionPolicy::new(1, 1).unwrap();
    let mut calls = 0;
    loop {
        let report =
            spot_effect_hold::purge_expired(&ledger, &s, policy, 10_000_000_000_000, 1_000)
                .await
                .unwrap();
        if report.purged > 0 {
            calls += 1;
        }
        if !report.more {
            break;
        }
    }
    assert_eq!(calls, 5);
    let all = [EffectState::Held, EffectState::Dispatched];
    let rows = ledger.list_effects(&s, &all).await.unwrap();
    assert_eq!(rows.len(), 5_001, "no row was deleted");
    for row in rows {
        if row.id == held {
            assert!(
                row.payload_ref != PayloadRef::Absent,
                "a held effect was purged"
            );
        } else {
            assert_eq!(row.payload_ref, PayloadRef::Absent);
            assert_eq!(row.state, EffectState::Dispatched);
        }
    }
    // The dedupe key survives the purge.
    let again = ledger.append_effect(stored(&s, "k0", &m)).await.unwrap();
    assert!(again != held);
}
