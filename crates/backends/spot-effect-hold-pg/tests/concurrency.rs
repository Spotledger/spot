//! WP-MIG-08.T2's concurrency test: many tasks appending to one site, and many `begin`s racing
//! on one site.
//!
//! The first half proves the ledger keeps every effect and one order: the sequence numbers are
//! `1..=N` with no gaps and no repeats, and `list_effects` returns them in that order. The second
//! proves the hold's compare-and-set has exactly one winner, so two nodes cannot both believe they
//! hold the site.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use spot_effect_hold::{EffectState, HoldState, LedgerStore};
use spot_postgres::testing::TestDb;

const APPENDS: usize = 32;
const RACERS: usize = 16;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_appends_keep_every_effect_in_one_order() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = Arc::new(ledger(&db, 16, "hold_concurrent_appends").await);
    let site = site("hold-concurrent-appends.example");
    let migration = mig("mig-1");

    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..APPENDS {
        let ledger = Arc::clone(&ledger);
        let site = site.clone();
        let migration = migration.clone();
        tasks.spawn(async move {
            let key = format!("k{index:02}");
            ledger
                .append_effect(effect(&site, &key, 1, &migration))
                .await
        });
    }
    let mut ids = HashSet::new();
    while let Some(joined) = tasks.join_next().await {
        ids.insert(joined.unwrap().unwrap());
    }
    assert_eq!(ids.len(), APPENDS, "every append got its own effect id");

    let listed = ledger
        .list_effects(&site, &[EffectState::Held])
        .await
        .unwrap();
    assert_eq!(listed.len(), APPENDS);
    let sequences: Vec<u64> = listed.iter().map(|record| record.sequence.0).collect();
    let expected: Vec<u64> = (1..=APPENDS as u64).collect();
    assert_eq!(
        sequences, expected,
        "the original order is 1..=N, with no gap and no repeated number"
    );
    let keys: HashSet<&str> = listed.iter().map(|record| record.key.as_str()).collect();
    assert_eq!(keys.len(), APPENDS);
    // Sequence order is the order the ledger committed the rows in; ids were assigned in a
    // different order, which is exactly why the sequence exists.
    let ordered_ids: HashSet<_> = listed.iter().map(|record| record.id).collect();
    assert_eq!(ordered_ids, ids);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racers_beginning_at_once_produce_exactly_one_hold() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = Arc::new(ledger(&db, 16, "hold_concurrent_cas").await);
    let site = site("hold-concurrent-cas.example");

    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..RACERS {
        let ledger = Arc::clone(&ledger);
        let site = site.clone();
        tasks.spawn(async move {
            let hold = held(&mig(&format!("mig-{index}")), 10);
            let won = ledger
                .compare_and_set_hold(&site, &HoldState::Open, hold.clone())
                .await
                .unwrap();
            (won, hold)
        });
    }
    let mut winners = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (won, hold) = joined.unwrap();
        if won {
            winners.push(hold);
        }
    }
    assert_eq!(winners.len(), 1, "exactly one node takes the hold");
    assert_eq!(
        ledger.get_hold(&site).await.unwrap(),
        winners[0],
        "the stored hold is the winner's, whole"
    );

    // Once held, no later `Open -> Held` compare-and-set can win, however long it waits.
    assert!(
        !ledger
            .compare_and_set_hold(&site, &HoldState::Open, held(&mig("mig-late"), 11))
            .await
            .unwrap()
    );
}
