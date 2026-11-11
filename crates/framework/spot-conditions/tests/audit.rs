//! One audit event per transition (ADR-061 section 2, ADR-067 section 1), including the event
//! count for a hundred-step flapping sequence.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    AuditEvent, ConditionDef, ConditionEngine, ConditionParams, ConditionTopic, MemorySink, Millis,
    Registry, Severity, Transition,
};

const WINDOW: u64 = 60_000;

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
    ConditionEngine::new(registry, MemorySink::new())
}

fn count(events: &[AuditEvent], transition: Transition) -> usize {
    events
        .iter()
        .filter(|event| event.transition() == transition)
        .count()
}

#[test]
fn a_lifecycle_is_exactly_one_event_per_transition() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(10_000))
        .unwrap();
    engine.tick(ms(10_000 + WINDOW));

    let events = engine.sink().events();
    assert_eq!(events.len(), 2);
    assert_eq!(count(events, Transition::Raised), 1);
    assert_eq!(count(events, Transition::Cleared), 1);
    assert_eq!(count(events, Transition::Escalated), 0);
    assert_eq!(events[0].topic(), ConditionTopic::Raised);
    assert_eq!(events[1].topic(), ConditionTopic::Cleared);
    assert_eq!(events[0].topic().as_str(), "sys_condition_raised");
    assert_eq!(events[1].topic().as_str(), "sys_condition_cleared");
}

#[test]
fn a_hundred_rapid_flaps_produce_one_condition_and_no_pair_of_events() {
    // One step a second, alternating raise and clear: a hundred steps of flapping, every one of
    // them inside the sixty-second debounce window.
    let mut engine = engine();
    for step in 0..100 {
        let now = ms(step * 1_000);
        if step % 2 == 0 {
            engine
                .raise("NODE_SILENT", "branch-12", ConditionParams::new(), now)
                .unwrap();
        } else {
            engine
                .observe_cleared("NODE_SILENT", "branch-12", now)
                .unwrap();
        }
        engine.tick(now);
    }

    // The condition was raised once and never cleared: one condition, not a hundred, and no
    // cleared/raised pair in the audit.
    assert_eq!(engine.sink().len(), 1);
    assert_eq!(engine.sink().events()[0].transition(), Transition::Raised);
    // The last step observed a clear, so it is pending, not open.
    assert!(engine.open().is_empty());
    assert_eq!(engine.history("branch-12").len(), 1);

    // Once the window closes, the one clear is recorded: two events for a hundred steps.
    engine.tick(ms(99 * 1_000 + WINDOW));
    assert_eq!(engine.sink().len(), 2);
    assert_eq!(
        engine.sink().events()[1].transition(),
        Transition::Cleared,
        "the clear is recorded at the instant it was observed"
    );
    assert_eq!(engine.sink().events()[1].at_millis(), ms(99 * 1_000));
}

#[test]
fn a_hundred_step_flapping_sequence_beyond_the_window_is_one_event_per_transition() {
    // Two minutes a step, alternating raise and clear: each flap is longer than the window, so
    // every transition is real. Fifty raises and fifty clears, and nothing else.
    let mut engine = engine();
    let step_millis = 2 * WINDOW;
    for step in 0..100u64 {
        let now = ms(step * step_millis);
        if step % 2 == 0 {
            engine
                .raise("NODE_SILENT", "branch-12", ConditionParams::new(), now)
                .unwrap();
        } else {
            engine
                .observe_cleared("NODE_SILENT", "branch-12", now)
                .unwrap();
        }
        engine.tick(now);
    }
    // The last step observed a clear: let its window close.
    engine.tick(ms(99 * step_millis + step_millis));

    let events = engine.sink().events();
    assert_eq!(events.len(), 100, "one event per transition");
    assert_eq!(count(events, Transition::Raised), 50);
    assert_eq!(count(events, Transition::Cleared), 50);
    // The events alternate raised, cleared, raised, ... : no two transitions collapsed.
    for (index, event) in events.iter().enumerate() {
        let expected = if index % 2 == 0 {
            Transition::Raised
        } else {
            Transition::Cleared
        };
        assert_eq!(event.transition(), expected, "event {index}");
    }
    assert!(engine.open().is_empty());
}

#[test]
fn the_events_of_one_key_are_in_transition_order() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(1_000))
        .unwrap();
    engine.tick(ms(1_000 + WINDOW));
    engine
        .raise(
            "NODE_SILENT",
            "branch-12",
            ConditionParams::new(),
            ms(200_000),
        )
        .unwrap();

    let times: Vec<u64> = engine
        .sink()
        .events()
        .iter()
        .map(|event| event.at_millis().as_u64())
        .collect();
    assert_eq!(times, vec![0, 1_000, 200_000]);
}
