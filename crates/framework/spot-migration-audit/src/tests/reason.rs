//! The `reason` of a finish record (WP-MIG-11.T3): written, read back, shown by `explain_evidence`,
//! and absent in a record written before the field existed.

use futures::executor::block_on;
use spot_audit_chain::{Payload, PayloadValue};

use super::{SITE, recorder, summary};
use crate::events::{FinishEvent, RunSpec};
use crate::explain::explain_evidence;
use crate::recorder::RunRecorder;
use crate::run::{ArtifactKind, ArtifactRef, Outcome, PlanRef, RunId};
use crate::store::{ChainRunStore, RunStore};

fn spec(id: &str) -> RunSpec {
    RunSpec {
        id: RunId::new(id).unwrap(),
        plan_digest: "d".repeat(64),
        plan: PlanRef {
            artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/p.json", b"plan"),
            summary: summary(),
        },
        actor: "user:alice".to_owned(),
        at: 1_000,
    }
}

fn finish(recorder: &impl RunRecorder, id: &str, outcome: Outcome, reason: Option<&str>) {
    let handle = block_on(recorder.begin(spec(id))).unwrap();
    block_on(recorder.finish(
        &handle,
        FinishEvent {
            outcome,
            at: 2_000,
            reason: reason.map(str::to_owned),
        },
    ))
    .unwrap();
}

#[test]
fn the_reason_round_trips_and_explain_names_it() {
    let (recorder, _ring) = recorder(1);
    finish(
        &recorder,
        "run-a",
        Outcome::Refused,
        Some("migration.not_permitted"),
    );
    let store = ChainRunStore::new(recorder.store(), SITE);
    let evidence = block_on(store.get(&RunId::new("run-a").unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(
        evidence.reason().as_deref(),
        Some("migration.not_permitted")
    );
    let text = explain_evidence(&evidence).render();
    assert!(text.contains("Reason migration.not_permitted"), "{text}");
}

#[test]
fn a_success_and_an_old_record_have_no_reason() {
    let (recorder, _ring) = recorder(2);
    finish(&recorder, "run-ok", Outcome::Verified, None);
    let store = ChainRunStore::new(recorder.store(), SITE);
    let evidence = block_on(store.get(&RunId::new("run-ok").unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(evidence.reason(), None);
    assert!(!explain_evidence(&evidence).render().contains("Reason"));

    // A stored v1 finish record: only `outcome` in the payload. It decodes as `None`.
    let mut payload = Payload::new();
    payload.insert(
        "outcome".to_owned(),
        PayloadValue::Text("failed".to_owned()),
    );
    assert!(!payload.contains_key("reason"));
    let v1 = crate::events::finished_record_for_test("user:alice", "run-v1", 3_000, payload);
    let decoded = crate::events::decode(&v1).unwrap().unwrap();
    assert!(matches!(
        decoded,
        crate::events::Event::Finished {
            outcome: Outcome::Failed,
            ..
        }
    ));
}
