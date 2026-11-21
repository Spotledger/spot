//! The invariants that must hold after **any** sequence of operations (ADR-067 section 1):
//! at most one open condition per `(id, subject)`, the open set is exactly the keys that were
//! raised and not cleared, severity only ever rises while a condition stays open, a debounced
//! raise keeps the severity it had, and the in-memory history says exactly what the audit sink
//! was told.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use spot_conditions::{
    ConditionDef, ConditionEngine, ConditionParams, Escalation, MemorySink, Millis, Open,
    RaiseOutcome, Registry, Severity,
};

/// Four keys over three definitions and two subjects, so a sequence mixes de-duplication, two
/// subjects of one condition and two conditions of one subject.
const KEYS: [(&str, &str); 4] = [
    ("NODE_SILENT", "n1"),
    ("NODE_SILENT", "n2"),
    ("JOURNAL_GAP", "s1"),
    ("LAG_HIGH", "n1"),
];

const SUBJECTS: [&str; 3] = ["n1", "n2", "s1"];

fn def_severity(key: usize) -> Severity {
    match key {
        2 => Severity::Red,
        _ => Severity::Amber,
    }
}

fn engine() -> ConditionEngine<MemorySink> {
    let mut registry = Registry::new();
    registry
        .register(
            ConditionDef::new(
                "NODE_SILENT",
                Severity::Amber,
                "node",
                "condition.node_silent",
            )
            .with_escalation(Escalation::new(Millis::new(30_000), Severity::Red)),
        )
        .unwrap();
    registry
        .register(ConditionDef::new(
            "JOURNAL_GAP",
            Severity::Red,
            "stream",
            "condition.journal_gap",
        ))
        .unwrap();
    registry
        .register(
            ConditionDef::new("LAG_HIGH", Severity::Amber, "node", "condition.lag")
                .with_escalation(Escalation::new(Millis::new(10_000), Severity::Red))
                .with_escalation(Escalation::new(Millis::new(20_000), Severity::Red)),
        )
        .unwrap();
    ConditionEngine::new(registry, MemorySink::new())
}

/// One generated step: raise a key, observe it clear, or tick. `delta` advances the clock before
/// the step, so the instants the engine sees are non-decreasing.
fn steps() -> impl Strategy<Value = Vec<(u8, usize, u64)>> {
    proptest::collection::vec((0u8..3, 0usize..KEYS.len(), 0u64..600_000), 0..40)
}

fn index_of(condition: &Open) -> Option<usize> {
    KEYS.iter().position(|(id, subject)| {
        condition.id.as_str() == *id && condition.subject.as_str() == *subject
    })
}

/// The severity of every open condition, by key index.
fn open_map(open: &[Open]) -> BTreeMap<usize, Severity> {
    open.iter()
        .filter_map(|condition| index_of(condition).map(|index| (index, condition.severity)))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn the_open_set_is_the_set_of_raised_and_not_cleared_keys(
        steps in steps(),
    ) {
        let mut engine = engine();
        let mut now = 0u64;
        // A tick never changes which keys are open, only what is recorded about them, so the
        // model only has to follow the raise and clear requests.
        let mut last_said_raise: BTreeMap<usize, bool> = BTreeMap::new();
        // The severity each key had when the previous step ended, and the severity of the
        // condition the last clear observed.
        let mut was_open: BTreeMap<usize, Severity> = BTreeMap::new();
        let mut went_clear_at: BTreeMap<usize, Severity> = BTreeMap::new();

        for (kind, key, delta) in steps {
            now += delta;
            let at = Millis::new(now);
            let mut raised = None;
            match kind {
                0 => {
                    let outcome = engine
                        .raise(KEYS[key].0, KEYS[key].1, ConditionParams::new(), at)
                        .unwrap();
                    last_said_raise.insert(key, true);
                    raised = Some((key, outcome));
                }
                1 => {
                    engine.observe_cleared(KEYS[key].0, KEYS[key].1, at).unwrap();
                    last_said_raise.insert(key, false);
                    if let Some(severity) = was_open.get(&key) {
                        went_clear_at.insert(key, *severity);
                    }
                }
                _ => {
                    engine.tick(at);
                }
            }

            let open = engine.open();
            let expected: BTreeSet<usize> = last_said_raise
                .iter()
                .filter(|(_, is_raised)| **is_raised)
                .map(|(key, _)| *key)
                .collect();
            let current = open_map(&open);
            prop_assert_eq!(
                current.keys().copied().collect::<BTreeSet<usize>>(),
                expected,
                "the open set after step ({}, {}, {})", kind, key, now
            );
            prop_assert_eq!(
                open.len(),
                current.len(),
                "one open condition per (id, subject)"
            );

            for (index, severity) in &current {
                prop_assert!(
                    *severity >= def_severity(*index),
                    "a condition is never below its definition's severity"
                );
                if let Some(before) = was_open.get(index) {
                    // Still open: an escalation never lowers, and a re-raise never resets it.
                    prop_assert!(
                        severity >= before,
                        "severity fell from {before:?} to {severity:?} while open"
                    );
                }
            }

            match raised {
                // A new condition starts at the definition's severity.
                Some((key, RaiseOutcome::Raised)) => prop_assert_eq!(
                    current.get(&key).copied(),
                    Some(def_severity(key))
                ),
                // De-duplication changes the parameters and `last_seen`, never the severity.
                Some((key, RaiseOutcome::Updated)) => prop_assert_eq!(
                    current.get(&key).copied(),
                    was_open.get(&key).copied()
                ),
                // A debounced resurrection is the same condition: it keeps the severity it had
                // when it was observed clear, including an escalation.
                Some((key, RaiseOutcome::Debounced)) => prop_assert_eq!(
                    current.get(&key).copied(),
                    went_clear_at.get(&key).copied()
                ),
                None => {}
            }

            was_open = current;
        }
    }

    #[test]
    fn the_history_says_exactly_what_the_audit_sink_was_told(steps in steps()) {
        let mut engine = engine();
        let mut now = 0u64;
        for (kind, key, delta) in steps {
            now += delta;
            let at = Millis::new(now);
            match kind {
                0 => {
                    engine
                        .raise(KEYS[key].0, KEYS[key].1, ConditionParams::new(), at)
                        .unwrap();
                }
                1 => {
                    engine.observe_cleared(KEYS[key].0, KEYS[key].1, at).unwrap();
                }
                _ => {
                    engine.tick(at);
                }
            }
        }

        let events = engine.sink().events();
        for subject in SUBJECTS {
            let told: Vec<_> = events
                .iter()
                .filter(|event| event.subject().as_str() == subject)
                .collect();
            let history = engine.history(subject);
            prop_assert_eq!(history.len(), told.len(), "history of {}", subject);
            for (entry, event) in history.iter().zip(told) {
                prop_assert_eq!(entry.id.as_str(), event.id().as_str());
                prop_assert_eq!(entry.transition, event.transition());
                prop_assert_eq!(entry.severity, event.severity());
                prop_assert_eq!(entry.at_millis, event.at_millis());
            }
        }
    }
}
