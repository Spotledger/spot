//! Upcasting: a chain across schema versions, and a missing step found up front.

use serde_json::{Value, json};
use spot_image_upcast::{Registry, Upcaster, add_field_with_default, rename_field};
use spot_journal_rebuild_plan::Target;

use super::block_on;
use super::fixture::{Ev, Journal, Model, insert, model, options, uuid};
use crate::{Call, RebuildError, RecordingSink, execute};

fn schema_event(n: u64, version: u64, schema: u64, body: Value) -> Ev {
    Ev::Insert {
        entity: "account",
        n,
        version,
        schema,
        body,
    }
}

/// 1 -> 2 renames `name` to `title`; 2 -> 3 adds `status`; 3 -> 4 renames `status` to `state`.
fn registry(up_to: u64) -> Registry {
    let mut registry = Registry::new();
    let steps = [
        Upcaster::new("account", 1, 2, rename_field("name", "title")),
        Upcaster::new(
            "account",
            2,
            3,
            add_field_with_default("status", json!("open")),
        ),
        Upcaster::new("account", 3, 4, rename_field("status", "state")),
    ];
    for step in steps.into_iter().filter(|s| s.to() <= up_to) {
        registry.register(step).unwrap();
    }
    registry
}

#[test]
fn images_of_three_schema_versions_reach_the_target_through_the_chain() {
    let events = vec![
        schema_event(1, 1, 1, json!({"name": "old"})),
        schema_event(2, 1, 2, json!({"title": "mid"})),
        schema_event(3, 1, 3, json!({"title": "new", "status": "closed"})),
        schema_event(4, 1, 4, json!({"title": "cur", "state": "open"})),
    ];
    let journal = Journal::new(events, 4);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &registry(4),
        &plan,
        &mut sink,
        &options(4),
    ))
    .unwrap();
    let state = sink.state().unwrap();
    let body = |n: u64| Value::Object(state[&("account".to_owned(), uuid(n))].image.body().clone());
    assert_eq!(body(1), json!({"title": "old", "state": "open"}));
    assert_eq!(body(2), json!({"title": "mid", "state": "open"}));
    assert_eq!(body(3), json!({"title": "new", "state": "closed"}));
    assert_eq!(body(4), json!({"title": "cur", "state": "open"}));
    assert!(state.values().all(|r| r.image.entity_schema() == 4));
}

#[test]
fn a_missing_step_is_reported_before_anything_is_applied() {
    // The only image at the old schema is the last entry; the sink must still see nothing.
    let mut events: Vec<Ev> = (1..=6)
        .map(|n| schema_event(n, 1, 3, json!({"title": "t", "status": "s"})))
        .collect();
    events.push(schema_event(7, 1, 1, json!({"name": "old"})));
    let journal = Journal::new(events, 4);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    // Steps 1->2 and 2->3 exist; 3->4 does not, and the target is 4.
    let error = block_on(execute(
        &journal.store,
        &journal.keys(),
        &registry(3),
        &plan,
        &mut sink,
        &options(4),
    ))
    .unwrap_err();
    assert_eq!(
        error,
        RebuildError::MissingUpcast {
            entity: "account".into(),
            from: 3,
            to: 4
        }
    );
    assert_eq!(error.code(), "journal_rebuild.missing_upcast");
    assert!(sink.calls().is_empty());
}

#[test]
fn a_missing_step_for_a_snapshot_image_is_found_from_the_index() {
    let events = vec![insert(1, 1), insert(2, 1), insert(3, 1)];
    let journal = Journal::new(events.clone(), 3);
    let snapshot = journal.snapshot_of(&model(&events, 2), 2, 1);
    let plan = journal.plan(vec![snapshot], Target::End);
    let mut sink = RecordingSink::new();
    let mut opts = options(2);
    opts.prescan = false;
    let error = block_on(execute(
        &journal.store,
        &journal.keys(),
        &Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap_err();
    assert_eq!(
        error,
        RebuildError::MissingUpcast {
            entity: "account".into(),
            from: 1,
            to: 2
        }
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn snapshot_images_are_upcast_too() {
    let events = vec![
        schema_event(1, 1, 1, json!({"name": "a"})),
        schema_event(2, 1, 1, json!({"name": "b"})),
    ];
    let journal = Journal::new(events.clone(), 2);
    let state: Model = model(&events, 2);
    let snapshot = journal.snapshot_of(&state, 2, 1);
    let plan = journal.plan(vec![snapshot], Target::End);
    let mut sink = RecordingSink::new();
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &registry(2),
        &plan,
        &mut sink,
        &options(2),
    ))
    .unwrap();
    let images: Vec<&Call> = sink
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Image(_)))
        .collect();
    assert_eq!(images.len(), 2);
    for call in images {
        let Call::Image(image) = call else {
            unreachable!()
        };
        assert_eq!(image.image.entity_schema(), 2);
        assert!(image.image.field("title").is_some());
        assert_eq!(image.position.seq, 2);
    }
}

#[test]
fn an_image_newer_than_the_target_is_refused_up_front() {
    let events = vec![schema_event(1, 1, 5, json!({"title": "future"}))];
    let journal = Journal::new(events, 1);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let error = block_on(execute(
        &journal.store,
        &journal.keys(),
        &registry(4),
        &plan,
        &mut sink,
        &options(4),
    ))
    .unwrap_err();
    assert_eq!(
        error,
        RebuildError::UpcastFailed {
            entity: "account".into(),
            code: "image_upcast.from_the_future"
        }
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn per_entity_targets_override_the_default() {
    let events = vec![schema_event(1, 1, 1, json!({"name": "a"}))];
    let journal = Journal::new(events, 1);
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let mut opts = options(9);
    opts.entity_targets.insert("account".to_owned(), 2);
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &registry(2),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap();
    let state = sink.state().unwrap();
    assert!(state.values().all(|r| r.image.entity_schema() == 2));
}
