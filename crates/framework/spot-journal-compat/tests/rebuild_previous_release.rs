//! The frozen journal, rebuilt by today's code, must equal the committed golden state.
//!
//! This is the ADR-061 s6 test: a journal written by the previous release — its segments, its
//! signed manifest and its signed snapshot — is planned and executed by today's planner, today's
//! executor and today's upcaster registry, and the state that reaches the sink is compared with
//! `golden-state.json`, byte for byte in canonical form. Nothing in the fixture is regenerated at
//! test time: the bytes under `fixtures/release-1/` are the release's own.
//!
//! The second test asks the same question of every prefix: a point-in-time rebuild to any
//! position must equal the golden checkpoint of that position, so the harness pins the journal's
//! *history*, not only its end state. Below the snapshot's position the planner has no base and
//! replays the first segment from position 1; from there on it restores the snapshot and rolls
//! forward — both must land on the same-state answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_journal_compat::{
    Fixture, LAST_POSITION, Record, SNAPSHOT_POSITION, TARGET_SCHEMA, block_on,
    default_fixture_dir, today,
};
use spot_journal_rebuild::{RebuildOutcome, RecordingSink, execute};
use spot_journal_rebuild_plan::{Base, Target};

/// The committed fixture.
fn fixture() -> Fixture {
    Fixture::load(&default_fixture_dir()).expect("the committed fixture loads")
}

/// Runs the rebuild of `fixture` to `target` with today's registry, and returns the outcome and
/// what the sink was handed.
fn rebuild(fixture: &Fixture, target: Target) -> (RebuildOutcome, RecordingSink) {
    let plan = fixture.plan(target).expect("the fixture makes a plan");
    let registry = today().expect("today's upcaster table registers");
    let keys = fixture.rebuild_keys();
    let options = fixture.options().expect("the fixture's options are usable");
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &fixture.store,
        &keys,
        &registry,
        &plan,
        &mut sink,
        &options,
    ))
    .expect("the frozen journal rebuilds");
    (outcome, sink)
}

/// What the sink committed, in the golden state's order.
fn committed(sink: &RecordingSink) -> Vec<Record> {
    let state = sink.state().expect("the rebuild committed");
    let mut records: Vec<Record> = state
        .iter()
        .map(|((entity, id), recorded)| {
            Record::of_sink(entity, id, recorded.version, &recorded.image)
        })
        .collect();
    records.sort_by(|a, b| (&a.entity, &a.id).cmp(&(&b.entity, &b.id)));
    records
}

#[test]
fn the_frozen_journal_rebuilds_to_the_golden_state() {
    let fixture = fixture();
    let plan = fixture
        .plan(Target::End)
        .expect("the journal plans to its end");
    assert_eq!(
        plan.ends_at, LAST_POSITION,
        "the plan ends at the last position"
    );
    assert_eq!(
        plan.base,
        Base::Snapshot {
            position: SNAPSHOT_POSITION,
            generation: 1
        },
        "a rebuild to the end restores the verified snapshot and rolls forward"
    );
    assert!(
        plan.preserves_positions,
        "a rebuild to the end keeps positions"
    );
    assert!(
        plan.fork_point.is_none(),
        "a rebuild to the end does not fork"
    );

    let (outcome, sink) = rebuild(&fixture, Target::End);
    assert_eq!(outcome.last_position.seq, LAST_POSITION);
    assert_eq!(
        outcome.snapshot_images, 4,
        "four records were in the snapshot"
    );
    assert_eq!(outcome.entries_read, 8, "positions 6..=13 were read");
    assert_eq!(
        outcome.state_applied, 5,
        "five state images followed the snapshot"
    );
    assert_eq!(outcome.deletes_applied, 1, "one tombstone was applied");
    assert_eq!(outcome.audit_applied, 1, "one audit entry was replayed");
    assert_eq!(
        outcome.domain_skipped, 1,
        "one domain event was not replayed"
    );
    assert_eq!(outcome.duplicates_skipped, 0, "nothing was delivered twice");

    let records = committed(&sink);
    for record in &records {
        assert_eq!(
            record.schema, TARGET_SCHEMA,
            "{} {} reached the target schema",
            record.entity, record.id
        );
    }
    assert_eq!(
        records, fixture.golden.last.records,
        "the rebuilt state is the golden state"
    );

    // The personal field travelled untouched: the sealed envelope the release wrote is what the
    // sink stored, with no key material and no plaintext beside it.
    let account = records
        .iter()
        .find(|record| record.entity == "account")
        .expect("the surviving account is in the rebuilt state");
    let contact = account
        .body
        .get("contact")
        .and_then(|value| value.as_object())
        .expect("the sealed envelope is still an object");
    assert_eq!(
        contact.get("sealed").and_then(|value| value.as_str()),
        Some("spot.journal.pii.v1")
    );
    assert!(
        contact
            .get("ciphertext")
            .is_some_and(|value| value.is_string()),
        "the ciphertext is opaque JSON, carried as stored"
    );
    assert!(
        contact.get("plaintext").is_none(),
        "the harness cannot see the plaintext of a sealed field"
    );
    assert!(
        !contact.keys().any(|key| key.contains("private")),
        "no key material is in the fixture or in what the sink stored"
    );
}

#[test]
fn every_prefix_rebuilds_to_the_golden_checkpoint() {
    let fixture = fixture();
    for position in 1..=LAST_POSITION {
        let plan = fixture
            .plan(Target::At(position))
            .unwrap_or_else(|error| panic!("position {position} plans: {error}"));
        let from_empty = matches!(plan.base, Base::Empty);
        assert_eq!(
            from_empty,
            position < SNAPSHOT_POSITION,
            "position {position} starts from the first segment before the snapshot, from the \
             snapshot after it"
        );
        assert_eq!(plan.ends_at, position);
        assert_eq!(
            plan.fork_point.is_some(),
            position < LAST_POSITION,
            "a rebuild before the end forks the stream"
        );

        let (outcome, sink) = rebuild(&fixture, Target::At(position));
        assert_eq!(outcome.last_position.seq, position);
        let expected = fixture
            .golden
            .checkpoint(position)
            .unwrap_or_else(|| panic!("the golden state has a checkpoint at {position}"));
        assert_eq!(
            committed(&sink),
            expected.records,
            "the rebuild to position {position} is the golden state of that position"
        );
    }
}
