//! A shape change with no upcaster must stop the rebuild **before it applies anything**.
//!
//! The executor promises this (ADR-061 s6): a journal an entity's chain cannot carry is refused
//! up front, because a half-rebuilt database is worse than none. The only way to prove it is to
//! take a step out of today's table and watch the rebuild refuse, with the sink never called —
//! not even `begin` — and no state committed.
//!
//! The journal is always the same frozen one: the full table rebuilds it (the control), and each
//! omitted step is the only difference.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_journal_compat::{Fixture, block_on, default_fixture_dir, registry_without, today};
use spot_journal_rebuild::{RebuildError, RecordingSink, execute};
use spot_journal_rebuild_plan::Target;

/// The committed fixture.
fn fixture() -> Fixture {
    Fixture::load(&default_fixture_dir()).expect("the committed fixture loads")
}

/// Runs the rebuild of `fixture` with `omitted` steps left out of the upcaster table, and returns
/// the error it refuses with — or panics when it does not refuse.
fn refuse(fixture: &Fixture, omitted: &[(&str, u64)]) -> (RebuildError, RecordingSink) {
    let registry = registry_without(omitted).expect("the trimmed table registers");
    let plan = fixture.plan(Target::End).expect("the journal plans");
    let keys = fixture.rebuild_keys();
    let options = fixture.options().expect("the options are usable");
    let mut sink = RecordingSink::new();
    let error = block_on(execute(
        &fixture.store,
        &keys,
        &registry,
        &plan,
        &mut sink,
        &options,
    ))
    .expect_err("the rebuild refuses a journal no chain can carry");
    (error, sink)
}

#[test]
fn the_full_table_rebuilds_the_same_frozen_journal() {
    // The control: the very journal the other tests refuse rebuilds when nothing is missing, so
    // each refusal below is caused by the omitted step and not by the fixture.
    let fixture = fixture();
    let registry = today().expect("today's table registers");
    let plan = fixture.plan(Target::End).expect("the journal plans");
    let keys = fixture.rebuild_keys();
    let options = fixture.options().expect("the options are usable");
    let mut sink = RecordingSink::new();
    block_on(execute(
        &fixture.store,
        &keys,
        &registry,
        &plan,
        &mut sink,
        &options,
    ))
    .expect("the full table rebuilds the frozen journal");
    assert_eq!(
        sink.state().map(|state| state.len()),
        Some(4),
        "four records are live at the end of the journal"
    );
}

#[test]
fn a_missing_snapshot_step_refuses_before_anything_is_applied() {
    let fixture = fixture();
    // `invoice` 2 → 3 is gone; the snapshot holds an invoice image at schema 2.
    let (error, sink) = refuse(&fixture, &[("invoice", 2)]);
    assert_eq!(error.code(), "journal_rebuild.missing_upcast");
    match &error {
        RebuildError::MissingUpcast { entity, from, to } => {
            assert_eq!(entity, "invoice");
            assert_eq!(
                *from, 2,
                "the chain stops at the schema the image was written under"
            );
            assert_eq!(*to, 3, "the target the rebuild asks for");
        }
        other => panic!("expected a missing upcaster, got {other:?}"),
    }
    assert_eq!(
        sink.calls().len(),
        0,
        "the sink was never called, so nothing was applied and nothing was begun"
    );
    assert!(sink.state().is_none(), "no state was committed");
    assert!(sink.aborted().is_none(), "there was nothing to abort");
}

#[test]
fn a_missing_stream_step_refuses_before_anything_is_applied() {
    let fixture = fixture();
    // `account` 1 → 2 is gone: the snapshot's account images sit at schema 1, and so do the
    // entries of the first segment. Either check refuses; both refuse before the sink.
    let (error, sink) = refuse(&fixture, &[("account", 1)]);
    assert_eq!(error.code(), "journal_rebuild.missing_upcast");
    match &error {
        RebuildError::MissingUpcast { entity, from, to } => {
            assert_eq!(entity, "account");
            assert_eq!(*from, 1);
            assert_eq!(*to, 3);
        }
        other => panic!("expected a missing upcaster, got {other:?}"),
    }
    assert!(sink.calls().is_empty());
    assert!(sink.state().is_none());
}
