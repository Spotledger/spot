//! Debounce against flapping (ADR-067 section 1): one condition, not many, and the boundary at
//! exactly the interval.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ClearOutcome, ConditionDef, ConditionEngine, ConditionParams, MemorySink, Millis, RaiseOutcome,
    Registry, Severity,
};

/// The default window, 60 seconds (ADR-067 section 1).
const WINDOW: u64 = 60_000;

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

fn engine_with(window: Millis) -> ConditionEngine<MemorySink> {
    let mut registry = Registry::new();
    registry
        .register(ConditionDef::new(
            "NODE_SILENT",
            Severity::Amber,
            "node",
            "condition.node_silent",
        ))
        .unwrap();
    ConditionEngine::with_debounce(registry, MemorySink::new(), window)
}

fn engine() -> ConditionEngine<MemorySink> {
    engine_with(ms(WINDOW))
}

#[test]
fn the_default_window_is_sixty_seconds() {
    assert_eq!(
        spot_conditions::DEFAULT_DEBOUNCE_MILLIS,
        ms(WINDOW),
        "ADR-067 section 1: a condition that clears and returns within 60 s is one condition"
    );
    assert_eq!(engine().debounce(), ms(WINDOW));
}

#[test]
fn a_condition_that_returns_inside_the_window_is_the_same_condition() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(10_000))
        .unwrap();

    // Back one millisecond inside the window: the same condition, with no cleared/raised pair
    // in the audit.
    assert_eq!(
        engine
            .raise(
                "NODE_SILENT",
                "branch-12",
                ConditionParams::new().with("silent_hours", "31"),
                ms(10_000 + WINDOW - 1),
            )
            .unwrap(),
        RaiseOutcome::Debounced
    );
    assert_eq!(engine.sink().len(), 1, "one raised event and nothing else");
    let open = engine.open();
    assert_eq!(open.len(), 1);
    assert_eq!(
        open[0].raised_at_millis,
        ms(0),
        "the condition never restarted"
    );
    assert_eq!(
        open[0].last_seen_millis,
        ms(10_000 + WINDOW - 1),
        "but it was seen again"
    );

    // Nothing is left pending to be recorded later either.
    assert_eq!(engine.tick(ms(10_000_000)).cleared, 0);
    assert_eq!(engine.sink().len(), 1);
}

#[test]
fn at_exactly_the_window_end_the_clear_is_recorded_and_a_new_condition_opens() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(10_000))
        .unwrap();

    // Exactly one window after the clear was observed: the window is half-open, [0, WINDOW), so
    // the condition is new.
    assert_eq!(
        engine
            .raise(
                "NODE_SILENT",
                "branch-12",
                ConditionParams::new(),
                ms(10_000 + WINDOW)
            )
            .unwrap(),
        RaiseOutcome::Raised
    );

    let events = engine.sink().events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].transition().as_str(), "raised");
    assert_eq!(events[0].at_millis(), ms(0));
    assert_eq!(events[1].transition().as_str(), "cleared");
    assert_eq!(
        events[1].at_millis(),
        ms(10_000),
        "the clear is recorded where it was observed"
    );
    assert_eq!(events[2].transition().as_str(), "raised");
    assert_eq!(events[2].at_millis(), ms(10_000 + WINDOW));

    assert_eq!(engine.open()[0].raised_at_millis, ms(10_000 + WINDOW));
}

#[test]
fn a_pending_clear_commits_when_a_tick_closes_the_window() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(10_000))
        .unwrap();

    assert_eq!(engine.tick(ms(10_000 + WINDOW - 1)), Default::default());
    assert_eq!(engine.sink().len(), 1);

    assert_eq!(engine.tick(ms(10_000 + WINDOW)).cleared, 1);
    assert_eq!(engine.sink().len(), 2);
    assert_eq!(engine.sink().events()[1].at_millis(), ms(10_000));

    // Committing twice changes nothing: the key is gone.
    assert_eq!(engine.tick(ms(10_000_000)).cleared, 0);
    assert_eq!(engine.sink().len(), 2);
}

#[test]
fn a_clear_that_never_comes_back_is_recorded_once() {
    let mut engine = engine();
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    assert_eq!(
        engine
            .observe_cleared("NODE_SILENT", "branch-12", ms(0))
            .unwrap(),
        ClearOutcome::Pending
    );
    engine.tick(ms(WINDOW));
    assert!(engine.open().is_empty());
    assert!(engine.history("branch-12").len() == 2);
    assert_eq!(
        engine.history("branch-12")[1].transition.as_str(),
        "cleared"
    );
}

#[test]
fn a_zero_window_commits_the_clear_at_the_next_tick() {
    // A window of zero turns debouncing off, which is what a caller that wants the raw
    // transition stream configures.
    let mut engine = engine_with(Millis::ZERO);
    engine
        .raise("NODE_SILENT", "branch-12", ConditionParams::new(), ms(0))
        .unwrap();
    engine
        .observe_cleared("NODE_SILENT", "branch-12", ms(5_000))
        .unwrap();
    assert_eq!(engine.tick(ms(5_000)).cleared, 1);
    assert_eq!(engine.sink().len(), 2);
}
