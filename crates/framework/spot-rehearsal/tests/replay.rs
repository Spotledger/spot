//! The replay policy: `off`, `always`, and `auto` resolved against the plan and the declaration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_rehearsal::{NoDeclaration, PlanDeclaration, ReplayPolicy, decide_replay};
use support::{empty_plan, plan_with_behaviour_change};

#[test]
fn off_never_replays() {
    let plan = plan_with_behaviour_change("behaviour/x");
    let decision = decide_replay(ReplayPolicy::Off, &plan, &PlanDeclaration);
    assert!(!decision.replay);
    assert!(decision.triggers.is_empty());
}

#[test]
fn always_always_replays() {
    let plan = empty_plan();
    let decision = decide_replay(ReplayPolicy::Always, &plan, &NoDeclaration);
    assert!(decision.replay);
}

#[test]
fn auto_triggers_on_a_behaviour_change() {
    let plan = plan_with_behaviour_change("behaviour/rounding");
    let decision = decide_replay(ReplayPolicy::Auto, &plan, &PlanDeclaration);
    assert!(decision.replay);
    assert_eq!(decision.triggers, vec!["behaviour/rounding".to_owned()]);
}

#[test]
fn auto_does_not_trigger_when_nothing_is_declared() {
    let plan = plan_with_behaviour_change("behaviour/rounding");
    let decision = decide_replay(ReplayPolicy::Auto, &plan, &NoDeclaration);
    assert!(!decision.replay);
    assert!(decision.triggers.is_empty());
}

#[test]
fn auto_does_not_trigger_on_an_empty_plan() {
    let plan = empty_plan();
    let decision = decide_replay(ReplayPolicy::Auto, &plan, &PlanDeclaration);
    assert!(!decision.replay);
}
