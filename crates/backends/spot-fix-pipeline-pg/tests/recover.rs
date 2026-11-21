//! `recover` over the PostgreSQL journal after every kill point of the engine reaches the same
//! state, world and journal as over the in-memory journal.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Kind, Rig, fixture, node};
use spot_fix_pipeline::{FixId, InMemoryJournal, KillAt, KillPoint, NoFaults};

fn points() -> Vec<(Kind, KillPoint)> {
    let mut out = Vec::new();
    for point in [
        KillPoint::BeforeProposed,
        KillPoint::AfterProposed,
        KillPoint::BeforePlanned,
        KillPoint::AfterPlanned,
        KillPoint::BeforeApply,
        KillPoint::BeforePreImages,
        KillPoint::AfterPreImages,
        KillPoint::BeforeStep(0),
        KillPoint::AfterStep(0),
        KillPoint::BeforeStep(1),
        KillPoint::AfterStep(1),
        KillPoint::BeforeApplied,
        KillPoint::AfterApplied,
        KillPoint::BeforeVerify,
        KillPoint::AfterVerify,
    ] {
        out.push((Kind::Small, point));
    }
    for point in [
        KillPoint::BeforeVerify,
        KillPoint::BeforeRollback,
        KillPoint::BeforeResidue,
        KillPoint::AfterRollback,
    ] {
        out.push((Kind::SymptomFails, point));
    }
    for point in [
        KillPoint::BeforeSnapshot,
        KillPoint::AfterSnapshot,
        KillPoint::BeforePreImages,
        KillPoint::AfterPreImages,
    ] {
        out.push((Kind::Structural, point));
    }
    out
}

#[test]
fn recovery_over_postgres_matches_the_in_memory_journal_at_every_kill_point() {
    let Some(fx) = fixture() else { return };
    let pg = node(&fx, "fix_recover");
    let mut killed = 0;
    for (n, (kind, point)) in points().into_iter().enumerate() {
        let fix = FixId::new(&format!("fix-recover-{n}")).unwrap();
        let faults = KillAt::new(point.clone());
        let memory = Rig::new(kind).scenario(&InMemoryJournal::new(), &fix, &faults);
        let postgres = Rig::new(kind).scenario(&pg, &fix, &faults);
        assert_eq!(memory, postgres, "kill point {point:?}");
        if postgres.recovered.is_some() {
            killed += 1;
        }
    }
    assert!(killed >= 15, "the kill points were reached ({killed})");
}

#[test]
fn a_run_without_faults_over_postgres_matches_the_in_memory_journal() {
    let Some(fx) = fixture() else { return };
    let pg = node(&fx, "fix_nofault");
    for (n, kind) in [Kind::Small, Kind::SymptomFails, Kind::Structural]
        .into_iter()
        .enumerate()
    {
        let fix = FixId::new(&format!("fix-clean-{n}")).unwrap();
        let memory = Rig::new(kind).scenario(&InMemoryJournal::new(), &fix, &NoFaults);
        let postgres = Rig::new(kind).scenario(&pg, &fix, &NoFaults);
        assert_eq!(memory, postgres);
        assert!(postgres.entries.len() > 3);
    }
}

#[test]
fn recovery_works_after_the_store_is_dropped_and_rebuilt_on_the_same_database() {
    let Some(fx) = fixture() else { return };
    let fix = FixId::new("fix-restart").unwrap();
    let mut rig = Rig::new(Kind::Small);
    {
        let first = node(&fx, "fix_restart_a");
        let faults = KillAt::new(KillPoint::AfterStep(0));
        assert!(rig.run(&first, &fix, &faults).is_err());
    }
    // A new bridge and journal (a restarted process) over the same database.
    let second = node(&fx, "fix_restart_b");
    let outcome = rig.recover(&second, &fix).unwrap();
    assert!(matches!(
        outcome.state.to_string().as_str(),
        "verified" | "rolled_back"
    ));
}
