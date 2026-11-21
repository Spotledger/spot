//! 300 seeded sequences on the in-memory fake, the shrinker proof, and a replay.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{calls_of, run_many, sequences};
use spot_l3_model::Model;
use spot_l3_model::model::{Bug, Call, Ver, Who};
use spot_l3_model::runner::run_sequence;
use spot_l3_model::shrink::{Check, shrink};
use spot_storage_fake::FakeBackend;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_hundred_sequences_agree_with_the_model_on_the_fake() {
    run_many(sequences(300), || Box::pin(async { FakeBackend::new() })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_sequence_replays_the_same_way() {
    let calls = calls_of(common::DEFAULT_SEED);
    let a = run_sequence(FakeBackend::new(), &calls, Model::new()).await;
    let b = run_sequence(FakeBackend::new(), &calls, Model::new()).await;
    assert_eq!(a, b);
    assert!(a.is_ok(), "{a:?}");
}

/// A model that thinks a draft may be cancelled is caught, and the failing sequence shrinks to
/// the two calls that matter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shrinker_finds_the_two_call_sequence_of_a_wrong_model() {
    let mut found = None;
    for seed in 0..200 {
        let calls = calls_of(seed);
        if run_sequence(
            FakeBackend::new(),
            &calls,
            Model::with_bug(Bug::CancelOfDraftAllowed),
        )
        .await
        .is_err()
        {
            found = Some(calls);
            break;
        }
    }
    let calls = found.expect("some seed cancels a draft");
    let check: Check<'_> = Box::new(|c: Vec<Call>| {
        Box::pin(async move {
            run_sequence(
                FakeBackend::new(),
                &c,
                Model::with_bug(Bug::CancelOfDraftAllowed),
            )
            .await
            .is_err()
        })
    });
    let small = shrink(calls, check).await;
    assert!(small.len() <= 3, "{small:?}");
    assert!(small.iter().any(|c| matches!(
        c,
        Call::Cancel {
            ver: Ver::Current,
            who: Who::Manager,
            ..
        }
    )));
}
