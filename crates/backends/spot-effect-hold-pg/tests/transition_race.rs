//! WP-MIG-08.T3: two writers racing one held effect on PostgreSQL.
//!
//! Each round appends an effect, lets two independent connections read it as `Held`, then has
//! both try to move it at the same instant: one to `Releasing` (a release), the other to
//! `Releasing` (a second release) or to `Discarded` (a discard). Without the `status = expected`
//! guard in the `UPDATE` both would win; with it exactly one does and the other is `Stale`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_effect_hold::{EffectId, EffectState, LedgerStore, Transition};
use spot_postgres::testing::TestDb;
use tokio::sync::Barrier;

const ROUNDS: usize = 24;

async fn state(ledger: &dyn LedgerStore, site: &spot_core::SiteId, id: EffectId) -> EffectState {
    let all = [
        EffectState::Held,
        EffectState::Releasing,
        EffectState::Dispatched,
        EffectState::Discarded,
        EffectState::NeedsConfirmation,
    ];
    ledger
        .list_effects(site, &all)
        .await
        .unwrap()
        .into_iter()
        .find(|record| record.id == id)
        .unwrap()
        .state
}

/// Two nodes, each with its own pool (its own connections), both read the effect as `Held`
/// before either writes.
async fn two_writers(first: EffectState, second: EffectState, name: &str) {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let node_a = Arc::new(ledger(&db, 2, "hold_race_a").await);
    let node_b = Arc::new(ledger(&db, 2, "hold_race_b").await);
    let site = site(name);
    let migration = mig("mig-1");

    for round in 0..ROUNDS {
        let id = node_a
            .append_effect(effect(&site, &format!("k{round}"), 1, &migration))
            .await
            .unwrap();
        // Both read it as Held.
        assert_eq!(state(node_a.as_ref(), &site, id).await, EffectState::Held);
        assert_eq!(state(node_b.as_ref(), &site, id).await, EffectState::Held);

        let start = Arc::new(Barrier::new(2));
        let mut tasks = tokio::task::JoinSet::new();
        for (node, next) in [(Arc::clone(&node_a), first), (Arc::clone(&node_b), second)] {
            let start = Arc::clone(&start);
            tasks.spawn(async move {
                start.wait().await;
                let outcome = node
                    .transition_effect(id, EffectState::Held, next, None)
                    .await
                    .unwrap();
                (next, outcome)
            });
        }
        let mut winners = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let (next, outcome) = joined.unwrap();
            match outcome {
                Transition::Applied => winners.push(next),
                Transition::Stale { actual } => assert_ne!(
                    actual,
                    EffectState::Held,
                    "a stale transition reports the state that beat it"
                ),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: exactly one writer wins");
        assert_eq!(
            state(node_a.as_ref(), &site, id).await,
            winners[0],
            "round {round}: the ledger holds the winner's state"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_releases_of_one_effect_have_one_winner() {
    two_writers(
        EffectState::Releasing,
        EffectState::Releasing,
        "hold-race-release.example",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_and_a_discard_of_one_effect_have_one_winner() {
    two_writers(
        EffectState::Releasing,
        EffectState::Discarded,
        "hold-race-discard.example",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_transition_changes_nothing_and_the_winner_counts_one_attempt() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 4, "hold_race_attempts").await;
    let site = site("hold-race-attempts.example");
    let id = ledger
        .append_effect(effect(&site, "k1", 1, &mig("mig-1")))
        .await
        .unwrap();
    assert!(
        ledger
            .transition_effect(id, EffectState::Held, EffectState::Releasing, None)
            .await
            .unwrap()
            .is_applied()
    );
    let stale = ledger
        .transition_effect(
            id,
            EffectState::Held,
            EffectState::Discarded,
            Some("too late".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(
        stale,
        Transition::Stale {
            actual: EffectState::Releasing
        }
    );
    let listed = ledger
        .list_effects(&site, &[EffectState::Releasing])
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].attempts, 1);
    assert_eq!(listed[0].note, None, "a stale transition wrote no note");
}
