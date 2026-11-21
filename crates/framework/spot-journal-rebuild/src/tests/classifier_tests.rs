//! The classifier seam and the registry default: the registry table, a custom classifier,
//! unregistered topics (refused, listed, or applied for a tenant in recovery) and the outcome.

use spot_events_contract::registry::{Class, REGISTRY, REGISTRY_LEN};
use spot_journal_rebuild_plan::Target;

use super::block_on;
use super::fixture::{Ev, Journal, insert, options, uuid};
use crate::{
    EventClass, EventClassifier, MAX_UNKNOWN_TOPICS, RebuildError, RebuildOutcome, RecordingSink,
    RegistryClassifier, UNKNOWN_CLASS, WARN_UNREGISTERED_AS_STATE, execute,
};

fn run(
    journal: &Journal,
    opts: &crate::RebuildOptions<'_>,
    sink: &mut RecordingSink,
) -> Result<RebuildOutcome, RebuildError> {
    let plan = journal.plan(Vec::new(), Target::End);
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        sink,
        opts,
    ))
}

/// Marks one topic as state and defers to the registry for the rest.
struct InventoryIsState;

impl EventClassifier for InventoryIsState {
    fn class_of(&self, topic: &str) -> Option<EventClass> {
        if topic == "inventory_adjusted" {
            Some(EventClass::State)
        } else {
            RegistryClassifier.class_of(topic)
        }
    }
}

#[test]
fn every_registry_topic_is_classified_exactly_as_the_registry_says() {
    assert_eq!(REGISTRY.len(), REGISTRY_LEN);
    for entry in &REGISTRY {
        let want = match entry.class {
            Class::State => EventClass::State,
            Class::Domain => EventClass::Domain,
            Class::Audit => EventClass::Audit,
        };
        assert_eq!(
            RegistryClassifier.class_of(entry.topic),
            Some(want),
            "{}",
            entry.topic
        );
    }
    // A name that looks like an audit topic is not one unless the registry says so.
    assert_eq!(RegistryClassifier.class_of("sys_made_up_by_a_guess"), None);
    assert_eq!(
        RegistryClassifier.class_of("stock_below_reorder_level"),
        None
    );
    assert_eq!(RegistryClassifier.class_of(""), None);
}

#[test]
fn a_custom_classifier_can_make_a_topic_state_and_it_is_replayed() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("inventory_adjusted")], 4);
    let classifier = InventoryIsState;
    let mut opts = options(1);
    opts.classifier = Some(&classifier);
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &opts, &mut sink).unwrap();
    assert_eq!(outcome.state_applied, 2);
    assert!(
        sink.state()
            .unwrap()
            .contains_key(&("account".to_owned(), uuid(9_500)))
    );
    assert!(outcome.unknown_topics.is_empty());
    assert_eq!(outcome.classes.get("state"), Some(&2));
}

#[test]
fn identity_audit_events_survive_a_rebuild_and_are_counted_separately() {
    let journal = Journal::new(
        vec![
            insert(1, 1),
            Ev::Topic("identity_user_created"),
            Ev::Audit,
            Ev::Topic("doc_posted"),
        ],
        4,
    );
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &options(1), &mut sink).unwrap();
    assert_eq!(outcome.state_applied, 1);
    assert_eq!(outcome.audit_applied, 2);
    assert_eq!(outcome.identity_audit_applied, 1);
    assert_eq!(outcome.domain_skipped, 1);
    let topics: Vec<&str> = sink.audits().iter().map(|a| a.topic.as_str()).collect();
    assert_eq!(topics, ["identity_user_created", "sys_doc_audit"]);
    assert_eq!(outcome.classes.get("state"), Some(&1));
    assert_eq!(outcome.classes.get("audit"), Some(&2));
    assert_eq!(outcome.classes.get("domain"), Some(&1));
    assert!(outcome.unknown_topics.is_empty());
    assert!(outcome.warnings.is_empty());
}

#[test]
fn a_domain_event_is_available_but_never_applied() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("doc_posted")], 4);
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &options(1), &mut sink).unwrap();
    assert_eq!(outcome.domain_skipped, 1);
    assert_eq!(outcome.state_applied, 1);
    assert!(
        !sink
            .state()
            .unwrap()
            .contains_key(&("account".to_owned(), uuid(9_500)))
    );
}

#[test]
fn an_unregistered_topic_is_refused_by_default_with_nothing_applied() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("mystery_happened")], 4);
    let mut sink = RecordingSink::new();
    let error = run(&journal, &options(1), &mut sink).unwrap_err();
    assert!(matches!(
        &error,
        RebuildError::UnknownTopic { generation: 1, seq: 2, topic } if topic == "mystery_happened"
    ));
    assert_eq!(error.code(), "journal_rebuild.unknown_topic");
    assert!(sink.calls().is_empty(), "the prescan refused before begin");
}

#[test]
fn without_the_prescan_the_refusal_aborts_the_sink() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("mystery_happened")], 4);
    let mut opts = options(1);
    opts.prescan = false;
    let mut sink = RecordingSink::new();
    let error = run(&journal, &opts, &mut sink).unwrap_err();
    assert_eq!(error.code(), "journal_rebuild.unknown_topic");
    assert_eq!(sink.aborted(), Some("journal_rebuild.unknown_topic"));
    assert!(sink.state().is_none(), "nothing was committed");
}

#[test]
fn with_strict_topics_off_an_unregistered_topic_is_listed_and_not_replayed() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("mystery_happened")], 4);
    let mut opts = options(1);
    opts.strict_topics = false;
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &opts, &mut sink).unwrap();
    assert_eq!(outcome.state_applied, 1);
    assert_eq!(outcome.domain_skipped, 1);
    assert_eq!(outcome.unknown_topics, ["mystery_happened"]);
    assert_eq!(outcome.classes.get(UNKNOWN_CLASS), Some(&1));
    assert!(outcome.warnings.is_empty());
}

#[test]
fn a_tenant_in_recovery_may_apply_an_unregistered_topic_as_state_with_a_warning() {
    let journal = Journal::new(vec![insert(1, 1), Ev::Topic("mystery_happened")], 4);
    let mut opts = options(1);
    opts.allow_unregistered = true;
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &opts, &mut sink).unwrap();
    assert_eq!(outcome.state_applied, 2);
    assert_eq!(outcome.unknown_topics, ["mystery_happened"]);
    assert_eq!(outcome.warnings, [WARN_UNREGISTERED_AS_STATE]);
    assert!(
        sink.state()
            .unwrap()
            .contains_key(&("account".to_owned(), uuid(9_500)))
    );
}

#[test]
fn a_hundred_and_one_unknown_topics_are_truncated_with_the_flag() {
    // Distinct topic names, leaked so they live for the whole test (the fixture takes `'static`).
    let topics: Vec<&'static str> = (0..=MAX_UNKNOWN_TOPICS)
        .map(|n| &*Box::leak(format!("unknown_topic_{n:03}").into_boxed_str()))
        .collect();
    let events: Vec<Ev> = topics.iter().copied().map(Ev::Topic).collect();
    let journal = Journal::new(events, 50);
    let mut opts = options(1);
    opts.strict_topics = false;
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &opts, &mut sink).unwrap();
    assert_eq!(outcome.unknown_topics.len(), MAX_UNKNOWN_TOPICS);
    assert!(outcome.unknown_topics_truncated);
    assert_eq!(outcome.unknown_topics.first().unwrap(), "unknown_topic_000");
    assert_eq!(outcome.unknown_topics.last().unwrap(), "unknown_topic_099");
    assert_eq!(outcome.classes.get(UNKNOWN_CLASS), Some(&101));
}

#[test]
fn an_older_outcome_json_still_loads() {
    let journal = Journal::new(vec![insert(1, 1)], 4);
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &options(1), &mut sink).unwrap();
    let mut value = serde_json::to_value(&outcome).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in [
        "unknown_topics",
        "unknown_topics_truncated",
        "classes",
        "identity_audit_applied",
        "warnings",
    ] {
        assert!(object.remove(key).is_some());
    }
    let loaded: RebuildOutcome = serde_json::from_value(value).unwrap();
    assert!(loaded.unknown_topics.is_empty());
    assert!(loaded.warnings.is_empty());
    assert_eq!(loaded.identity_audit_applied, 0);
    assert_eq!(loaded.entries_read, outcome.entries_read);
}
