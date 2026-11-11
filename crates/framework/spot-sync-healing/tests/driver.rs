//! Behaviour of the healing driver. Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_conditions::{ConditionEngine, MemorySink, Millis, Registry, Transition};
use spot_sync_healing::{
    ActionKind, ActionOutcome, Cause, ConditionSignal, HealingDriver, HealingJournal,
    HealingSettings, JournalOutcome, MemoryFrozen, MemoryJournal, Observation, RepairStatus,
    Target, apply_signals, register_conditions,
};
use spot_sync_reconcile::NodeRole;

fn driver(script: Script) -> HealingDriver<Script, MemoryJournal> {
    HealingDriver::new(script, MemoryJournal::new(), HealingSettings::default()).unwrap()
}

fn wide() -> HealingSettings {
    HealingSettings {
        max_in_flight: 100_000,
        ..HealingSettings::default()
    }
}

#[test]
fn each_finding_kind_gets_its_repair() {
    let mut d = driver(Script::default());
    let attempts = d.step(
        &[obs(
            "branch",
            vec![missing("A"), behind("B"), orphan("Z"), total()],
        )],
        ms(0),
    );
    assert_eq!(attempts.len(), 4);
    let kind_of = |t: &Target| attempts.iter().find(|a| &a.target == t).unwrap().kind;
    let record = |n: &str| Target::Record {
        entity: "item".into(),
        name: n.into(),
    };
    assert_eq!(kind_of(&record("A")), ActionKind::RepullRecord);
    assert_eq!(kind_of(&record("B")), ActionKind::RepullRecord);
    assert_eq!(kind_of(&record("Z")), ActionKind::RequestSnapshot);
    assert_eq!(kind_of(&Target::Scope), ActionKind::RebuildDigest);
    assert!(attempts.iter().all(|a| a.outcome == ActionOutcome::Done));
}

#[test]
fn a_burst_of_a_thousand_findings_is_rate_limited_globally_and_per_node() {
    let mut d = HealingDriver::new(Script::default(), MemoryJournal::new(), wide()).unwrap();
    let observations: Vec<Observation> = (0..10)
        .map(|n| {
            obs(
                &format!("node-{n}"),
                (0..100).map(|i| missing(&format!("R{i}"))).collect(),
            )
        })
        .collect();
    let first = d.step(&observations, ms(0));
    assert_eq!(first.len(), 30, "global limit");
    for n in 0..10 {
        let node = format!("node-{n}");
        assert!(first.iter().filter(|a| a.node == node).count() <= 5);
    }
    // Nothing more inside the same minute.
    assert!(d.step(&observations, ms(59_999)).is_empty());
    // A full minute later the window has emptied: another 30.
    assert_eq!(d.step(&observations, ms(60_000)).len(), 30);
}

#[test]
fn the_in_flight_bound_holds_back_new_repairs_until_they_verify_or_expire() {
    let settings = HealingSettings {
        max_in_flight: 3,
        max_repairs_per_node_per_minute: 100,
        max_repairs_per_minute: 100,
        ..HealingSettings::default()
    };
    let mut d = HealingDriver::new(Script::default(), MemoryJournal::new(), settings).unwrap();
    let all = obs("b", (0..10).map(|i| missing(&format!("R{i}"))).collect());
    assert_eq!(d.step(std::slice::from_ref(&all), ms(0)).len(), 3);
    assert!(d.step(std::slice::from_ref(&all), ms(1_000)).is_empty());
    // Three of them get fixed; the rest are still reported: three more may start.
    let rest = obs("b", (3..10).map(|i| missing(&format!("R{i}"))).collect());
    assert_eq!(d.step(&[rest], ms(2_000)).len(), 3);
}

#[test]
fn the_most_urgent_then_the_oldest_finding_goes_first() {
    let settings = HealingSettings {
        max_repairs_per_minute: 3,
        max_repairs_per_node_per_minute: 3,
        ..HealingSettings::default()
    };
    let mut d = HealingDriver::new(Script::default(), MemoryJournal::new(), settings).unwrap();
    // R1 is first seen at 0, then the rest appear at 1,000 (and the budget is only 3).
    let first = d.step(&[obs("b", vec![missing("R1")])], ms(0));
    assert_eq!(first.len(), 1);
    // Fix R1 by omission, then three arrive: an orphan, a record, an aggregate.
    let later = obs("b", vec![orphan("Z"), missing("R2"), total()]);
    let second = d.step(&[later], ms(1_000));
    let order: Vec<_> = second.iter().map(|a| a.target.clone()).collect();
    // Two slots left in the minute: the aggregate, then the record.
    assert_eq!(second.len(), 2);
    assert_eq!(order[0], Target::Scope);
    assert_eq!(
        order[1],
        Target::Record {
            entity: "item".into(),
            name: "R2".into()
        }
    );
}

#[test]
fn backoff_doubles_and_the_fourth_failure_stops_the_repair_and_raises_the_condition() {
    let mut d = driver(Script::failing("transport down"));
    let o = obs("branch", vec![missing("A")]);
    let one = std::slice::from_ref(&o);

    assert_eq!(d.step(one, ms(0)).len(), 1);
    assert!(matches!(
        d.status(ms(0))[0].status,
        RepairStatus::Backoff { until } if until == ms(10_000)
    ));
    assert!(
        d.step(one, ms(9_999)).is_empty(),
        "inside the first backoff"
    );
    assert_eq!(d.step(one, ms(10_000)).len(), 1);
    assert!(d.step(one, ms(29_999)).is_empty(), "20 s after the second");
    assert_eq!(d.step(one, ms(30_000)).len(), 1);
    assert!(d.step(one, ms(69_999)).is_empty(), "40 s after the third");
    assert!(
        d.take_signals().is_empty(),
        "not raised before the last attempt"
    );

    let fourth = d.step(one, ms(70_000));
    assert_eq!(fourth.len(), 1);
    assert_eq!(fourth[0].number, 4);
    assert_eq!(d.status(ms(70_000))[0].status, RepairStatus::Stopped);

    // Stopped: nothing more, however long we wait.
    assert!(d.step(one, ms(10_000_000)).is_empty());
    assert_eq!(d.actions().calls.len(), 4);

    let signals = d.take_signals();
    assert_eq!(signals.len(), 1);
    let ConditionSignal::Raise {
        subject,
        params,
        cause,
        evidence,
    } = &signals[0]
    else {
        panic!("expected a raise");
    };
    assert_eq!(subject, "branch/company=C1");
    assert_eq!(*cause, Cause::AttemptsExhausted);
    assert_eq!(params.get("cause"), Some("attempts_exhausted"));
    assert_eq!(evidence.len(), 4, "the journal is the evidence");
    assert!(
        evidence
            .iter()
            .all(|e| e.outcome == JournalOutcome::Failed("transport down".into()))
    );
}

#[test]
fn the_condition_clears_after_a_clean_comparison() {
    let mut d = driver(Script::failing("down"));
    let o = obs("branch", vec![missing("A")]);
    for t in [0, 10_000, 30_000, 70_000] {
        d.step(std::slice::from_ref(&o), ms(t));
    }
    assert_eq!(d.take_signals().len(), 1);

    // Still diverged on the next comparison: still stopped, no new signal.
    d.step(std::slice::from_ref(&o), ms(100_000));
    assert!(d.take_signals().is_empty());

    // An operator fixed it by hand: the comparison is clean.
    let clean = obs("branch", vec![]);
    assert!(d.step(&[clean], ms(200_000)).is_empty());
    assert_eq!(
        d.take_signals(),
        vec![ConditionSignal::Clear {
            subject: "branch/company=C1".into()
        }]
    );
    assert!(d.status(ms(200_000)).is_empty());
}

#[test]
fn signals_drive_a_conditions_engine_through_raise_and_clear() {
    let mut registry = Registry::new();
    register_conditions(&mut registry).unwrap();
    let mut engine = ConditionEngine::new(registry, MemorySink::new());
    let mut d = driver(Script::failing("down"));
    let o = obs("branch", vec![missing("A")]);
    for t in [0, 10_000, 30_000, 70_000] {
        d.step(std::slice::from_ref(&o), ms(t));
    }
    apply_signals(&mut engine, &d.take_signals(), ms(70_000)).unwrap();
    assert_eq!(engine.open().len(), 1);

    d.step(&[obs("branch", vec![])], ms(80_000));
    apply_signals(&mut engine, &d.take_signals(), ms(80_000)).unwrap();
    engine.tick(Millis::new(80_000 + 10 * 60_000));
    assert!(engine.open().is_empty());
    let events = engine.sink().events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].transition(), Transition::Raised);
}

#[test]
fn a_frozen_scope_is_never_repaired_and_shows_as_frozen() {
    let mut frozen = MemoryFrozen::new();
    frozen.freeze("branch", scope());
    let mut d = HealingDriver::new(
        Script::default(),
        MemoryJournal::new(),
        HealingSettings::default(),
    )
    .unwrap()
    .with_frozen(frozen);
    let o = obs("branch", vec![missing("A"), total()]);
    assert!(d.step(std::slice::from_ref(&o), ms(0)).is_empty());
    assert!(d.step(std::slice::from_ref(&o), ms(1_000_000)).is_empty());
    assert!(d.actions().calls.is_empty());
    assert!(d.journal().all().is_empty());
    assert!(
        d.status(ms(0))
            .iter()
            .all(|s| s.status == RepairStatus::Frozen)
    );
    assert_eq!(d.status(ms(0)).len(), 2);
    // Another node with the same scope is not frozen.
    assert_eq!(d.step(&[obs("other", vec![missing("A")])], ms(1)).len(), 1);

    // Thawed: the repair runs.
    d.frozen_mut().thaw("branch", &scope());
    assert_eq!(d.step(std::slice::from_ref(&o), ms(2)).len(), 2);
}

fn roles(source: Option<NodeRole>, destination: Option<NodeRole>) -> Observation {
    Observation {
        source_role: source,
        destination_role: destination,
        ..obs("hub", vec![missing("A"), total()])
    }
}

#[test]
fn direction_safety_per_role_only_owner_to_mirror_is_repaired() {
    use NodeRole::{Mirror, Owner};
    // A hub-owned record never goes from a node to the hub: that comparison has the node as
    // the (mirror) source and the hub as the (owner) destination.
    let refused = [
        roles(Some(Mirror), Some(Owner)),
        roles(Some(Mirror), Some(Mirror)),
        roles(Some(Owner), Some(Owner)),
        roles(None, Some(Mirror)),
        roles(Some(Owner), None),
        roles(None, None),
    ];
    for observation in refused {
        let mut d = driver(Script::default());
        let attempts = d.step(std::slice::from_ref(&observation), ms(0));
        assert!(attempts.is_empty(), "{observation:?}");
        assert!(d.actions().calls.is_empty());
        assert!(d.journal().all().is_empty());
        assert!(matches!(
            d.status(ms(0))[0].status,
            RepairStatus::Refused {
                code: "sync_healing.direction_ambiguous"
            }
        ));
        let signals = d.take_signals();
        assert!(matches!(
            &signals[..],
            [ConditionSignal::Raise {
                cause: Cause::Refused("sync_healing.direction_ambiguous"),
                ..
            }]
        ));
    }
    let mut d = driver(Script::default());
    assert_eq!(d.step(&[roles(Some(Owner), Some(Mirror))], ms(0)).len(), 2);
    assert!(d.take_signals().is_empty());
}

#[test]
fn a_node_owned_record_is_repaired_to_the_hub_when_the_node_is_the_owner() {
    use NodeRole::{Mirror, Owner};
    // Reversed for node-owned records (ADR-067 section 4): source is the node, an owner.
    let observation = Observation {
        source_role: Some(Owner),
        destination_role: Some(Mirror),
        ..obs("hub", vec![behind("N1")])
    };
    let mut d = driver(Script::default());
    let attempts = d.step(&[observation], ms(0));
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].node, "hub");
}

#[test]
fn a_mirror_newer_than_its_owner_is_not_repaired_and_raises_the_condition() {
    let mut d = driver(Script::default());
    let o = obs("branch", vec![ahead("A"), missing("B")]);
    assert!(d.step(&[o], ms(0)).is_empty());
    assert!(d.actions().calls.is_empty());
    assert!(matches!(
        d.status(ms(0))[0].status,
        RepairStatus::Refused {
            code: "sync_healing.mirror_ahead"
        }
    ));
    assert_eq!(d.take_signals().len(), 1);

    // Once the roles and versions are certain again the condition clears and repairs resume.
    let attempts = d.step(&[obs("branch", vec![missing("B")])], ms(1));
    assert_eq!(attempts.len(), 1);
    assert!(matches!(
        &d.take_signals()[..],
        [ConditionSignal::Clear { .. }]
    ));
}

#[test]
fn a_second_step_with_the_same_findings_and_no_time_does_nothing() {
    let mut d = driver(Script::default());
    let o = obs(
        "branch",
        vec![missing("A"), behind("B"), orphan("Z"), total()],
    );
    let first = d.step(std::slice::from_ref(&o), ms(5));
    assert_eq!(first.len(), 4);
    let calls = d.actions().calls.len();
    let entries = d.journal().all().len();
    assert!(d.step(std::slice::from_ref(&o), ms(5)).is_empty());
    assert_eq!(d.actions().calls.len(), calls);
    assert_eq!(d.journal().all().len(), entries);

    // Idempotent for a failing action too (it is in backoff).
    let mut d = driver(Script::failing("x"));
    assert_eq!(d.step(std::slice::from_ref(&o), ms(5)).len(), 4);
    assert!(d.step(std::slice::from_ref(&o), ms(5)).is_empty());

    // And when the rate limit cut the first pass short.
    let mut d = HealingDriver::new(Script::default(), MemoryJournal::new(), wide()).unwrap();
    let burst = obs("b", (0..50).map(|i| missing(&format!("R{i}"))).collect());
    assert_eq!(d.step(std::slice::from_ref(&burst), ms(0)).len(), 5);
    assert!(d.step(std::slice::from_ref(&burst), ms(0)).is_empty());
}

#[test]
fn a_repair_that_is_done_but_not_visible_in_the_next_comparison_counts_as_an_attempt() {
    let mut d = driver(Script::default());
    let o = obs("branch", vec![missing("A")]);
    let one = std::slice::from_ref(&o);
    assert_eq!(d.step(one, ms(0)).len(), 1);
    assert!(matches!(
        d.status(ms(0))[0].status,
        RepairStatus::InFlight { verify_at } if verify_at == ms(300_000)
    ));
    assert!(d.step(one, ms(299_999)).is_empty());
    // The verify window passes with the finding still reported: not verified, backoff 10 s.
    assert!(d.step(one, ms(300_000)).is_empty());
    assert!(matches!(
        d.status(ms(300_000))[0].status,
        RepairStatus::Backoff { .. }
    ));
    let again = d.step(one, ms(310_000));
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].number, 2);
    let outcomes: Vec<_> = d
        .journal()
        .all()
        .iter()
        .map(|e| e.outcome.clone())
        .collect();
    assert_eq!(
        outcomes,
        vec![
            JournalOutcome::Done,
            JournalOutcome::NotVerified,
            JournalOutcome::Done
        ]
    );
}

#[test]
fn a_repair_whose_finding_disappears_has_worked_and_leaves_no_state() {
    let mut d = driver(Script::default());
    d.step(&[obs("branch", vec![missing("A")])], ms(0));
    assert!(d.step(&[obs("branch", vec![])], ms(1_000)).is_empty());
    assert!(d.status(ms(1_000)).is_empty());
    assert!(d.take_signals().is_empty());
}

#[test]
fn an_action_that_is_not_supported_falls_through_to_the_next_wider_one() {
    let mut script = Script::default();
    script
        .by_kind
        .insert(ActionKind::RebuildDigest, ActionOutcome::NotSupported);
    script
        .by_kind
        .insert(ActionKind::ResendScope, ActionOutcome::NotSupported);
    let mut d = driver(script);
    let attempts = d.step(&[obs("b", vec![total()])], ms(0));
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].kind, ActionKind::RequestSnapshot);
    assert_eq!(attempts[0].outcome, ActionOutcome::Done);
    let kinds: Vec<_> = d.journal().all().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ActionKind::RebuildDigest,
            ActionKind::ResendScope,
            ActionKind::RequestSnapshot
        ]
    );

    // Nothing supported at all: a failed attempt that backs off.
    let all = Script::answering(ActionKind::RepullRecord, ActionOutcome::NotSupported);
    let mut all = {
        let mut s = all;
        s.by_kind
            .insert(ActionKind::RequestSnapshot, ActionOutcome::NotSupported);
        driver(s)
    };
    let attempts = all.step(&[obs("b", vec![missing("A")])], ms(0));
    assert_eq!(attempts[0].outcome, ActionOutcome::NotSupported);
    assert!(matches!(
        all.status(ms(0))[0].status,
        RepairStatus::Backoff { .. }
    ));
}

#[test]
fn the_journal_holds_keys_and_versions_only_and_shortens_a_reason() {
    let long = format!("secret-ish\n{}", "x".repeat(500));
    let mut d = driver(Script::failing(&long));
    d.step(&[obs("branch", vec![behind("B")])], ms(7));
    let entries = d.journal().entries("branch", &scope());
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.at, ms(7));
    assert_eq!(entry.version, Some(5));
    assert_eq!(
        entry.target,
        Target::Record {
            entity: "item".into(),
            name: "B".into()
        }
    );
    let JournalOutcome::Failed(reason) = &entry.outcome else {
        panic!("expected a failure");
    };
    assert!(reason.chars().count() <= 120);
    assert!(!reason.contains('\n'));
    assert!(d.journal().entries("other", &scope()).is_empty());
}

#[test]
fn two_scopes_back_off_independently() {
    let mut d = driver(Script::failing("down"));
    let a = obs("branch", vec![missing("A")]);
    let b = obs_in(
        "branch",
        spot_sync_reconcile::Scope::new("company", "C2"),
        vec![missing("A")],
    );
    d.step(std::slice::from_ref(&a), ms(0));
    // The other scope is not held back by the first one's backoff.
    assert_eq!(d.step(&[a, b], ms(1)).len(), 1);
}

#[test]
fn bad_settings_are_refused() {
    let bad = HealingSettings {
        max_in_flight: 0,
        ..HealingSettings::default()
    };
    let result = HealingDriver::new(Script::default(), MemoryJournal::new(), bad);
    assert_eq!(
        result.err().unwrap().code(),
        "sync_healing.invalid_settings"
    );
}
