//! The frozen journal may hold only registered event topics, with the classes its table says.
//!
//! The rebuild executor classifies every entry through the events contract registry and refuses a
//! topic the registry does not know (WP-JRN-05.T9). A fixture with an unregistered topic would turn
//! the one test that protects upgrades red for a reason that has nothing to do with upgrades, so
//! this test reads the **committed** segments and holds every topic in them to a table written
//! here. The table is the documentation of what the fixture holds: a topic the generator emits
//! without a row fails with the topic's name, and a row the fixture does not use fails too.
//!
//! The second half pins the executor's class rules to the frozen journal: the class counts of a
//! full rebuild equal the counts the table predicts. The last test freezes a throwaway journal
//! with one unregistered topic and shows the default rebuild refuses it, so the guard rests on a
//! refusal that is itself proven.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use spot_events_contract::identity;
use spot_events_contract::registry::{Class, class_of};
use spot_journal_compat::{
    Fixture, LAST_POSITION, SNAPSHOT_POSITION, block_on, default_fixture_dir, generator, today,
};
use spot_journal_rebuild::{RebuildError, RecordingSink, execute};
use spot_journal_rebuild_plan::Target;

/// Every topic the frozen journal holds and the class the registry must give it.
///
/// One row per topic: add a row here in the same change that makes the generator emit a topic.
const EXPECTED: &[(&str, Class)] = &[
    ("doc_state", Class::State),
    ("doc_posted", Class::Domain),
    ("identity_role_changed", Class::Audit),
];

fn fixture() -> Fixture {
    Fixture::load(&default_fixture_dir()).expect("the committed fixture loads")
}

fn expected(topic: &str) -> Option<Class> {
    EXPECTED
        .iter()
        .find(|(name, _)| *name == topic)
        .map(|(_, class)| *class)
}

#[test]
fn every_topic_in_the_frozen_journal_is_registered_with_the_expected_class() {
    let topics = fixture().journal_topics().expect("the segments read");
    assert_eq!(
        topics.len() as u64,
        LAST_POSITION,
        "the segments hold every position of the journal"
    );
    for (position, topic) in &topics {
        let registered = class_of(topic).unwrap_or_else(|| {
            panic!(
                "position {position}: the topic `{topic}` is not in the events registry; a fixture \
                 may hold only registered topics (use a registered topic in the generator)"
            )
        });
        let row = expected(topic).unwrap_or_else(|| {
            panic!(
                "position {position}: the topic `{topic}` has no row in EXPECTED; add one with its \
                 class"
            )
        });
        assert_eq!(
            registered, row,
            "the topic `{topic}` is registered with another class than this test's table says"
        );
    }
}

#[test]
fn the_table_has_no_row_the_fixture_does_not_use() {
    let used: BTreeSet<String> = fixture()
        .journal_topics()
        .expect("the segments read")
        .into_iter()
        .map(|(_, topic)| topic)
        .collect();
    for (topic, _) in EXPECTED {
        assert!(
            used.contains(*topic),
            "EXPECTED has a row for `{topic}` that the frozen journal never uses; remove the row"
        );
    }
}

#[test]
fn the_generators_identity_payloads_pass_the_contract_validator() {
    // The committed bytes are tied to the generator's script by the determinism test, so the
    // script is what the segments hold.
    for event in generator::events::script() {
        if event.topic.starts_with("identity_") {
            assert!(
                identity::validate(event.topic, &event.data).is_ok(),
                "position {}: the payload of `{}` fails the identity validator",
                event.position,
                event.topic
            );
        }
    }
}

#[test]
fn a_full_rebuild_counts_each_class_as_the_table_predicts() {
    let fixture = fixture();
    let topics = fixture.journal_topics().expect("the segments read");

    // The entries the rebuild reads are the ones after the snapshot's position.
    let mut by_class: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut identity_audit = 0_u64;
    for (position, topic) in topics.iter().filter(|(p, _)| *p > SNAPSHOT_POSITION) {
        let class = expected(topic).expect("every fixture topic has a row");
        let name = match class {
            Class::State => "state",
            Class::Domain => "domain",
            Class::Audit => "audit",
        };
        *by_class.entry(name).or_insert(0) += 1;
        if class == Class::Audit && topic.starts_with("identity_") {
            identity_audit += 1;
        }
        assert!(*position <= LAST_POSITION);
    }

    let plan = fixture.plan(Target::End).expect("the journal plans");
    let registry = today().expect("today's upcaster table registers");
    let keys = fixture.rebuild_keys();
    // The default options: no classifier injected, strict topics, no `allow_unregistered`.
    let options = fixture.options().expect("the fixture's options are usable");
    assert!(
        options.classifier.is_none(),
        "the default classifier is in force"
    );
    assert!(options.strict_topics && !options.allow_unregistered);
    let mut sink = RecordingSink::new();
    let outcome = block_on(execute(
        &fixture.store,
        &keys,
        &registry,
        &plan,
        &mut sink,
        &options,
    ))
    .expect("the frozen journal rebuilds with the default options");

    for class in ["state", "domain", "audit"] {
        assert_eq!(
            outcome.classes.get(class).copied().unwrap_or(0),
            by_class.get(class).copied().unwrap_or(0),
            "entries of class {class}"
        );
    }
    assert!(
        !outcome.classes.contains_key("unknown"),
        "no entry was unknown to the registry"
    );
    assert!(outcome.unknown_topics.is_empty());
    assert_eq!(
        outcome.state_applied + outcome.deletes_applied,
        by_class["state"],
        "every state entry after the snapshot was applied (images and tombstones)"
    );
    assert_eq!(
        outcome.domain_skipped, by_class["domain"],
        "a domain event is made available, not applied"
    );
    assert_eq!(
        outcome.audit_applied, by_class["audit"],
        "an audit entry is replayed"
    );
    assert_eq!(
        outcome.identity_audit_applied, identity_audit,
        "identity audit entries are counted separately"
    );
    assert!(
        outcome.warnings.is_empty(),
        "nothing was applied as unregistered state"
    );
}

#[test]
fn the_default_rebuild_refuses_a_journal_with_an_unregistered_topic() {
    // A throwaway journal: the frozen script with its last entry on the old, unregistered name.
    let mut script = generator::events::script();
    let last = script.last_mut().expect("the script has entries");
    assert_eq!(last.topic, "identity_role_changed");
    last.topic = "sys_role_changed";
    assert!(
        class_of("sys_role_changed").is_none(),
        "the old name is not registered, which is the premise of this test"
    );

    let generated = generator::build_from(&script).expect("the throwaway journal builds");
    let fixture = Fixture::from_generated(&generated).expect("the throwaway journal loads");
    let plan = fixture.plan(Target::End).expect("the journal plans");
    let registry = today().expect("today's upcaster table registers");
    let keys = fixture.rebuild_keys();
    let options = fixture.options().expect("the options are usable");
    let mut sink = RecordingSink::new();
    let result = block_on(execute(
        &fixture.store,
        &keys,
        &registry,
        &plan,
        &mut sink,
        &options,
    ));
    match result {
        Err(RebuildError::UnknownTopic { topic, seq, .. }) => {
            assert_eq!(topic, "sys_role_changed");
            assert_eq!(seq, LAST_POSITION);
        }
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("the default rebuild must refuse an unregistered topic"),
    }
}
