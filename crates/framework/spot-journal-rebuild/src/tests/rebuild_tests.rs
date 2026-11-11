//! Roll-forward, point in time, tombstones, classes.

use proptest::prelude::*;
use spot_events_contract::envelope::Position;
use spot_journal_rebuild_plan::{Base, Target};

use super::block_on;
use super::fixture::{Ev, Journal, delete, insert, model, options, update, uuid};
use crate::{Call, EventClass, RecordingSink, classify, execute};

/// What the sink holds, as the model holds it.
fn sink_state(sink: &RecordingSink) -> super::fixture::Model {
    sink.state()
        .unwrap()
        .iter()
        .map(|(key, recorded)| {
            let body = serde_json::Value::Object(recorded.image.body().clone());
            (key.clone(), (recorded.version, body))
        })
        .collect()
}

/// Thirty events over six accounts: inserts, updates, a delete, some domain and audit entries.
fn script() -> Vec<Ev> {
    let mut events = Vec::new();
    for n in 1..=6 {
        events.push(insert(n, 1));
    }
    events.push(Ev::Domain);
    for n in 1..=6 {
        events.push(update(n, 2));
    }
    events.push(Ev::Audit);
    events.push(delete(3, 2));
    for n in [1, 2, 4] {
        events.push(update(n, 3));
    }
    events.push(Ev::Domain);
    events.push(insert(7, 1));
    events.push(update(7, 2));
    events.push(insert(3, 5));
    events.push(update(5, 3));
    events.push(update(6, 3));
    events.push(Ev::Audit);
    events.push(update(7, 3));
    events.push(update(2, 4));
    events
}

#[test]
fn roll_forward_over_a_snapshot_and_two_segments_equals_the_source_state() {
    let events = script();
    let total = events.len();
    let journal = Journal::new(events.clone(), (total - 10).div_ceil(2));
    let snapshot = journal.snapshot(10);
    let plan = journal.plan(vec![snapshot], Target::End);
    assert!(matches!(plan.base, Base::Snapshot { position: 10, .. }));
    assert!(plan.preserves_positions);
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(sink_state(&sink), model(&events, total));
    assert_eq!(
        outcome.last_position,
        Position {
            generation: 1,
            seq: total as u64
        }
    );
    assert!(outcome.fork_point.is_none());
    assert!(outcome.preserves_positions);
    assert_eq!(outcome.snapshot_images, model(&events, 10).len() as u64);
    assert_eq!(outcome.entries_read, (total - 10) as u64);
    assert!(matches!(sink.calls().last(), Some(Call::Commit(p)) if *p == outcome.last_position));
    // Positions are preserved on each stored version.
    let recorded = &sink.state().unwrap()[&("account".to_owned(), uuid(2))];
    assert_eq!(
        recorded.position,
        Position {
            generation: 1,
            seq: total as u64
        }
    );
    assert_eq!(recorded.version, 4);
}

#[test]
fn rebuild_from_the_first_position_without_a_snapshot() {
    let events = script();
    let journal = Journal::new(events.clone(), 8);
    let plan = journal.plan(vec![], Target::End);
    assert_eq!(plan.base, Base::Empty);
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(sink_state(&sink), model(&events, events.len()));
    assert_eq!(outcome.snapshot_images, 0);
    assert_eq!(outcome.entries_read, events.len() as u64);
}

#[test]
fn a_backup_base_streams_no_images_and_rolls_forward() {
    let events = script();
    let journal = Journal::new(events.clone(), 8);
    let catalog = journal.catalog(vec![]);
    let backup = spot_journal_rebuild_plan::Backup { position: 12 };
    let plan = spot_journal_rebuild_plan::plan(&catalog, Some(&backup), Target::End).unwrap();
    assert_eq!(plan.base, Base::Backup { position: 12 });
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(outcome.snapshot_images, 0);
    assert_eq!(outcome.entries_read, (events.len() - 12) as u64);
}

#[test]
fn a_point_in_time_rebuild_forks_and_reports_it_without_writing_the_journal() {
    let events = script();
    let journal = Journal::new(events.clone(), 8);
    let snapshot = journal.snapshot(10);
    let before = block_on(spot_journal_store::JournalStore::list(
        &journal.store,
        "",
        None,
        1000,
    ))
    .unwrap();
    let plan = journal.plan(vec![snapshot], Target::At(15));
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(sink_state(&sink), model(&events, 15));
    assert_eq!(
        outcome.last_position,
        Position {
            generation: 1,
            seq: 15
        }
    );
    assert_eq!(outcome.new_generation, Some(2));
    let fork = outcome.fork_point.unwrap();
    assert_eq!((fork.from_position, fork.abandoned_generation), (15, 1));
    assert!(!outcome.preserves_positions);
    let after = block_on(spot_journal_store::JournalStore::list(
        &journal.store,
        "",
        None,
        1000,
    ))
    .unwrap();
    assert_eq!(before, after, "the executor writes nothing");
}

#[test]
fn a_tombstone_removes_the_record_and_a_later_insert_starts_again() {
    let events = vec![
        insert(1, 1),
        update(1, 2),
        delete(1, 2),
        insert(1, 1),
        insert(2, 1),
    ];
    let journal = Journal::new(events.clone(), 5);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(outcome.deletes_applied, 1);
    assert_eq!(sink_state(&sink), model(&events, 5));
    let deletes = sink
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Delete(d) if d.version == 2 && d.position.seq == 3))
        .count();
    assert_eq!(deletes, 1);
}

#[test]
fn only_state_and_audit_entries_reach_the_sink() {
    let events = vec![
        insert(1, 1),
        Ev::Domain,
        Ev::Audit,
        Ev::Domain,
        update(1, 2),
    ];
    let journal = Journal::new(events, 5);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    assert_eq!(
        (
            outcome.state_applied,
            outcome.audit_applied,
            outcome.domain_skipped
        ),
        (2, 1, 2)
    );
    assert_eq!(sink.audits().len(), 1);
    assert_eq!(sink.audits()[0].topic, "sys_role_changed");
    assert_eq!(sink.audits()[0].position.seq, 3);

    let mut sink = RecordingSink::new();
    let mut opts = options(1);
    opts.replay_audit = false;
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap();
    assert_eq!((outcome.audit_applied, outcome.audit_skipped), (0, 1));
    assert!(sink.audits().is_empty());
}

#[test]
fn classes_follow_the_reserved_topic_names() {
    assert_eq!(classify("doc_state"), EventClass::State);
    assert_eq!(classify("sys_login_failed"), EventClass::Audit);
    assert_eq!(classify("doc_posted"), EventClass::Domain);
    assert_eq!(classify("stock_below_reorder_level"), EventClass::Domain);
    assert_eq!(classify("identity_changed"), EventClass::Domain);
}

#[test]
fn progress_is_reported_every_n_items() {
    let events = script();
    let journal = Journal::new(events.clone(), 8);
    let snapshot = journal.snapshot(10);
    let plan = journal.plan(vec![snapshot], Target::End);
    let seen = std::sync::Mutex::new(Vec::new());
    let report = |p: crate::Progress| seen.lock().unwrap().push(p.processed);
    let mut opts = options(1);
    opts.progress = Some(&report);
    opts.progress_every = 7;
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap();
    let total = outcome.snapshot_images + outcome.entries_read;
    let expected: Vec<u64> = (1..=total / 7).map(|n| n * 7).collect();
    assert_eq!(*seen.lock().unwrap(), expected);
}

fn arb_script() -> impl Strategy<Value = Vec<Ev>> {
    // A consistent script: per record, versions rise; deletes keep the last version.
    proptest::collection::vec((1_u64..5, 0_u8..4), 1..40).prop_map(|steps| {
        let mut version = [0_u64; 5];
        let mut alive = [false; 5];
        let mut events = Vec::new();
        for (n, kind) in steps {
            let i = usize::try_from(n).unwrap();
            match kind {
                0 if alive[i] => {
                    events.push(delete(n, version[i]));
                    alive[i] = false;
                }
                1 => events.push(Ev::Domain),
                2 => events.push(Ev::Audit),
                _ => {
                    version[i] += 1;
                    events.push(if alive[i] {
                        update(n, version[i])
                    } else {
                        insert(n, version[i])
                    });
                    alive[i] = true;
                }
            }
        }
        events
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn roll_forward_equals_the_model_for_any_script(events in arb_script(), per in 1_usize..9, cut in 0_usize..40) {
        let journal = Journal::new(events.clone(), per);
        let cut = cut.min(events.len().saturating_sub(1));
        let snapshots = if cut > 0 { vec![journal.snapshot(cut as u64)] } else { vec![] };
        let plan = journal.plan(snapshots, Target::End);
        let mut sink = RecordingSink::new();
        block_on(execute(&journal.store, &journal.keys(), &spot_image_upcast::Registry::new(), &plan, &mut sink, &options(1))).unwrap();
        prop_assert_eq!(sink_state(&sink), model(&events, events.len()));
    }
}
