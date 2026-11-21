//! The calm-by-default health surface (ADR-067 section 6): counts per severity, the short list
//! that needs a person, and its order.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ConditionDef, ConditionEngine, ConditionParams, HealthSummary, HealthThresholds, MemorySink,
    Millis, Registry, Severity,
};

const HOUR: u64 = 3_600_000;

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

fn engine() -> ConditionEngine<MemorySink> {
    let mut registry = Registry::new();
    registry
        .register(ConditionDef::new(
            "NODE_SILENT",
            Severity::Amber,
            "node",
            "condition.node_silent",
        ))
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
        .register(ConditionDef::new(
            "EXTRAS_GROWTH",
            Severity::Info,
            "entity",
            "condition.extras_growth",
        ))
        .unwrap();
    ConditionEngine::new(registry, MemorySink::new())
}

fn four_hours() -> HealthThresholds {
    HealthThresholds::new(ms(4 * HOUR))
}

/// `(id, subject)` of the list, in the order the surface shows it.
fn listed(summary: &HealthSummary) -> Vec<(&str, &str)> {
    summary
        .needs_person
        .iter()
        .map(|condition| (condition.id.as_str(), condition.subject.as_str()))
        .collect()
}

#[test]
fn nothing_open_is_green_and_calm() {
    let engine = engine();
    let summary = engine.health(ms(0), &four_hours());
    assert_eq!(summary.counts.total(), 0);
    assert_eq!(summary.worst(), None);
    assert!(summary.is_calm());
    assert!(summary.needs_person.is_empty());
}

#[test]
fn the_colour_is_the_worst_severity_open() {
    let mut engine = engine();
    engine
        .raise("EXTRAS_GROWTH", "entity-1", ConditionParams::new(), ms(0))
        .unwrap();
    let summary = engine.health(ms(0), &four_hours());
    assert_eq!(summary.worst(), Some(Severity::Info));
    assert!(summary.is_calm(), "an info is not a person's problem");

    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(
        engine.health(ms(0), &four_hours()).worst(),
        Some(Severity::Amber)
    );

    engine
        .raise("JOURNAL_GAP", "stream-9", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(
        engine.health(ms(0), &four_hours()).worst(),
        Some(Severity::Red)
    );
}

#[test]
fn counts_are_per_severity_and_a_pending_clear_is_not_counted() {
    let mut engine = engine();
    engine
        .raise("EXTRAS_GROWTH", "entity-1", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-13", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .raise("JOURNAL_GAP", "stream-9", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("JOURNAL_GAP", "stream-9", ms(1_000))
        .unwrap();

    let summary = engine.health(ms(1_000), &four_hours());
    assert_eq!(summary.counts.info, 1);
    assert_eq!(summary.counts.amber, 2);
    assert_eq!(
        summary.counts.red, 0,
        "the red one is gone, its clear is pending"
    );
    assert_eq!(summary.counts.total(), 3);
    assert_eq!(summary.worst(), Some(Severity::Amber));
}

#[test]
fn every_red_condition_needs_a_person_at_once() {
    let mut engine = engine();
    engine
        .raise("JOURNAL_GAP", "stream-9", ConditionParams::new(), ms(0))
        .unwrap();
    let summary = engine.health(ms(1), &four_hours());
    assert!(!summary.is_calm());
    assert_eq!(listed(&summary), vec![("JOURNAL_GAP", "stream-9")]);
}

#[test]
fn an_amber_condition_joins_the_list_once_it_has_been_open_for_the_threshold() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();

    assert!(engine.health(ms(4 * HOUR - 1), &four_hours()).is_calm());
    let summary = engine.health(ms(4 * HOUR), &four_hours());
    assert!(
        !summary.is_calm(),
        "at the threshold it has waited long enough"
    );
    assert_eq!(listed(&summary), vec![("NODE_SILENT", "branch-12")]);
}

#[test]
fn the_list_is_worst_first_then_oldest_first() {
    let mut engine = engine();
    // Two reds, the second younger, and an amber that is older than both.
    engine
        .raise(
            "JOURNAL_GAP",
            "stream-9",
            ConditionParams::new(),
            ms(10_000),
        )
        .unwrap();
    engine
        .raise(
            "JOURNAL_GAP",
            "stream-8",
            ConditionParams::new(),
            ms(20_000),
        )
        .unwrap();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    // An amber that is young enough to stay off the list, and an info that never joins it.
    engine
        .raise(
            "NODE_SILENT",
            "branch-13",
            ConditionParams::new(),
            ms(4 * HOUR),
        )
        .unwrap();
    engine
        .raise("EXTRAS_GROWTH", "entity-1", ConditionParams::new(), ms(0))
        .unwrap();

    let summary = engine.health(ms(5 * HOUR), &four_hours());
    assert_eq!(
        listed(&summary),
        vec![
            ("JOURNAL_GAP", "stream-9"),
            ("JOURNAL_GAP", "stream-8"),
            ("NODE_SILENT", "branch-12"),
        ],
        "severity first, then the one that has waited longest"
    );
    assert_eq!(summary.counts.red, 2);
    assert_eq!(summary.counts.amber, 2);
    assert_eq!(summary.counts.info, 1);
}

#[test]
fn an_escalation_moves_a_condition_to_the_top_of_the_list() {
    let mut registry = Registry::new();
    registry
        .register(
            ConditionDef::new(
                "NODE_SILENT",
                Severity::Amber,
                "node",
                "condition.node_silent",
            )
            .with_escalation(spot_conditions::Escalation::new(ms(HOUR), Severity::Red)),
        )
        .unwrap();
    let mut engine = ConditionEngine::new(registry, MemorySink::new());
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();

    assert!(engine.health(ms(HOUR - 1), &four_hours()).is_calm());
    engine.tick(ms(HOUR));
    let summary = engine.health(ms(HOUR), &four_hours());
    assert!(!summary.is_calm());
    assert_eq!(summary.worst(), Some(Severity::Red));
    assert_eq!(listed(&summary), vec![("NODE_SILENT", "branch-12")]);
}

#[test]
fn a_summary_can_be_built_from_a_slice_the_caller_holds() {
    let summary = HealthSummary::of(&[], ms(0), &four_hours());
    assert_eq!(summary.counts.total(), 0);
    assert!(summary.is_calm());
}
