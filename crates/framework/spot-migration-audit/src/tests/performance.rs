//! Query performance on 10,000 runs.

use std::time::Instant;

use futures::executor::block_on;

use super::{SITE, recorder};
use crate::events::{FinishEvent, RunSpec};
use crate::recorder::RunRecorder;
use crate::run::{ArtifactKind, ArtifactRef, Outcome, PlanRef, RunFilter, RunId};
use crate::store::{ChainRunStore, RunStore};

fn spec(id: &str, actor: &str, at: i64) -> RunSpec {
    RunSpec {
        id: RunId::new(id).unwrap(),
        plan_digest: "plan-digest".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/x.json", b"plan"),
            summary: super::summary(),
        },
        actor: actor.to_owned(),
        at,
    }
}

#[test]
fn queries_stay_fast_on_ten_thousand_runs() {
    let (recorder, _ring) = recorder(11);
    for i in 0..10_000usize {
        let id = format!("run-{i:05}");
        let actor = if i % 2 == 0 { "user:alice" } else { "user:bob" };
        let handle = block_on(recorder.begin(spec(&id, actor, 1_000 + i as i64))).unwrap();
        if i % 2 == 0 {
            block_on(recorder.finish(
                &handle,
                FinishEvent {
                    outcome: Outcome::Verified,
                    at: 2_000 + i as i64,
                },
            ))
            .unwrap();
        }
    }

    let store = ChainRunStore::new(recorder.store(), SITE);

    // The first list folds the whole chain (20,000 entries).
    let first = Instant::now();
    let all = block_on(store.list(RunFilter::all())).unwrap();
    let first_elapsed = first.elapsed();
    assert_eq!(all.len(), 10_000);
    assert!(
        first_elapsed.as_secs() < 5,
        "the first list took {first_elapsed:?}"
    );

    // The later queries reuse the cached fold.
    let second = Instant::now();
    let verified = block_on(store.list(RunFilter {
        outcome: Some(Outcome::Verified),
        ..RunFilter::all()
    }))
    .unwrap();
    assert_eq!(verified.len(), 5_000);
    let by_actor = block_on(store.list(RunFilter {
        actor: Some("user:bob".to_owned()),
        ..RunFilter::all()
    }))
    .unwrap();
    assert_eq!(by_actor.len(), 5_000);
    let one = block_on(store.get(&RunId::new("run-05000").unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(one.run.id.as_str(), "run-05000");
    assert_eq!(one.run.actor, "user:alice");
    let cached_elapsed = second.elapsed();
    assert!(
        cached_elapsed.as_secs() < 1,
        "the cached queries took {cached_elapsed:?}"
    );
}
