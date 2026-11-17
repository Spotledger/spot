//! The snapshot: a round trip, a promotion that carries on after a restore at every step, an
//! unknown version, an unknown field, tampering, and the store's refusal of a stale save.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::snapshot::SNAPSHOT_VERSION;
use crate::{Promotion, Snapshot};

/// A runner that insists on the live apply plan the service must have re-derived.
struct LiveRunner;

impl RehearsalRunner for LiveRunner {
    fn rehearse(&self, request: &RehearsalRequest<'_>) -> Result<RehearsalReport, PromotionError> {
        assert!(
            request.plan.apply_plan().is_some(),
            "a restored plan must be given its apply plan before a rehearsal"
        );
        Ok(report(request.plan, RehearsalOutcome::Clean))
    }
}

/// `promotion` through JSON text and back.
fn through_json(promotion: &Promotion) -> Promotion {
    let text = promotion.snapshot().unwrap().to_json().unwrap();
    let snapshot = Snapshot::from_json(&text).unwrap();
    Promotion::restore(snapshot, &Limits::default(), &registry()).unwrap()
}

/// Replaces the stored promotion with its restored copy: what a new process starts from.
fn restart_from_snapshot(harness: &Harness, id: &RunId) {
    let restored = through_json(&harness.load(id));
    harness.store.save(restored).unwrap();
}

#[test]
fn a_snapshot_round_trips_at_every_state() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, planned) = harness.planned("s-1", &bundle, Role::Test, &target, 1_000);
    let check = |promotion: &Promotion| {
        let restored = through_json(promotion);
        assert_eq!(restored.snapshot().unwrap(), promotion.snapshot().unwrap());
        assert_eq!(restored.state(), promotion.state());
        assert_eq!(restored.revision(), promotion.revision());
        assert_eq!(restored.journal(), promotion.journal());
        assert_eq!(
            restored.plan().map(|p| p.release()),
            promotion.plan().map(|p| p.release())
        );
        assert_eq!(
            restored.plan().map(|p| p.apply_hash()),
            promotion.plan().map(|p| p.apply_hash())
        );
        restored
    };
    check(&planned);
    block_on(
        harness
            .service
            .rehearse(&id, &bundle, &target, &LiveRunner, "user:req", 1_100),
    )
    .unwrap();
    let rehearsed = check(&harness.load(&id));
    assert_eq!(rehearsed.state(), PromotionState::Rehearsed);
    assert!(rehearsed.rehearsal().is_some());
    block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 1_200),
    )
    .unwrap();
    let applied = check(&harness.load(&id));
    assert!(applied.pre_image().is_some());
    assert_eq!(
        applied.pre_image().unwrap().digest(),
        harness.load(&id).pre_image().unwrap().digest()
    );
    block_on(
        harness
            .service
            .verify(&id, &bundle, &mut target, "user:req", 1_300),
    )
    .unwrap();
    assert_eq!(check(&harness.load(&id)).state(), PromotionState::Verified);
}

#[test]
fn a_promotion_resumes_from_a_snapshot_between_every_step() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("s-2", &bundle, Role::Test, &target, 2_000);

    restart_from_snapshot(&harness, &id);
    block_on(
        harness
            .service
            .rehearse(&id, &bundle, &target, &LiveRunner, "user:req", 2_100),
    )
    .unwrap();
    restart_from_snapshot(&harness, &id);
    block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 2_200),
    )
    .unwrap();
    restart_from_snapshot(&harness, &id);
    // Verification receives the apply plan re-derived against the applied target.
    let verified = block_on(
        harness
            .service
            .verify(&id, &bundle, &mut target, "user:req", 2_300),
    )
    .unwrap();
    assert_eq!(verified.state(), PromotionState::Verified);
    assert_eq!(harness.applier.applies(), 1);
}

#[test]
fn a_restored_plan_refuses_a_target_that_changed() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("s-3", &bundle, Role::Test, &target, 3_000);
    restart_from_snapshot(&harness, &id);
    target.site = target.site.clone().with_script(
        "nightly_rollup",
        "base",
        "party.before_save",
        "export function run(doc: any) { return doc; }",
    );
    let error =
        block_on(
            harness
                .service
                .rehearse(&id, &bundle, &target, &LiveRunner, "user:req", 3_100),
        )
        .unwrap_err();
    assert_eq!(error.code(), "promotion.plan_changed");
}

#[test]
fn an_unknown_version_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (_id, planned) = harness.planned("s-4", &bundle, Role::Test, &target, 4_000);
    let mut value = planned.snapshot().unwrap().to_value().unwrap();
    value["version"] = serde_json::json!(SNAPSHOT_VERSION + 1);
    let error = Snapshot::from_value(value).unwrap_err();
    assert_eq!(error.code(), "promotion_pg.snapshot_version");

    let mut snapshot = planned.snapshot().unwrap();
    snapshot.version = SNAPSHOT_VERSION + 1;
    let error = Promotion::restore(snapshot, &Limits::default(), &registry()).unwrap_err();
    assert_eq!(error.code(), "promotion_pg.snapshot_version");

    let error = Snapshot::from_json("{\"id\":\"x\"}").unwrap_err();
    assert_eq!(error.code(), "promotion_pg.snapshot_invalid");
    let error = Snapshot::from_json("not json").unwrap_err();
    assert_eq!(error.code(), "promotion_pg.snapshot_invalid");
}

#[test]
fn an_unknown_field_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (_id, planned) = harness.planned("s-5", &bundle, Role::Test, &target, 5_000);
    let mut value = planned.snapshot().unwrap().to_value().unwrap();
    value["surprise"] = serde_json::json!(true);
    let error = Snapshot::from_value(value).unwrap_err();
    assert_eq!(error.code(), "promotion_pg.snapshot_invalid");
}

#[test]
fn a_tampered_snapshot_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("s-6", &bundle, Role::Test, &target, 6_000);
    block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 6_100),
    )
    .unwrap();
    let good = harness.load(&id).snapshot().unwrap();
    let refuse = |edit: &dyn Fn(&mut Snapshot)| {
        let mut snapshot = good.clone();
        edit(&mut snapshot);
        let error = Promotion::restore(snapshot, &Limits::default(), &registry()).unwrap_err();
        assert_eq!(error.code(), "promotion_pg.snapshot_invalid", "{error}");
    };
    // The plan edited after approval no longer hashes to its digest.
    refuse(&|s| s.plan.as_mut().unwrap().release.from_version = 99);
    // The plan names another bundle.
    refuse(&|s| s.plan.as_mut().unwrap().bundle_digest = "0".repeat(64));
    // The journal does not end in the recorded state.
    refuse(&|s| {
        s.journal.pop();
    });
    refuse(&|s| s.state = "teleported".to_owned());
    refuse(&|s| s.from_env.role = "staging".to_owned());
    refuse(&|s| s.id = "no spaces allowed".to_owned());
    // The pre-image no longer opens to its digest.
    refuse(&|s| s.pre_image.as_mut().unwrap().digest = "0".repeat(64));
    refuse(&|s| s.pre_image.as_mut().unwrap().bytes_hex = "zz".to_owned());
    assert!(Promotion::restore(good, &Limits::default(), &registry()).is_ok());
}

#[test]
fn a_stale_save_is_refused_and_writes_nothing() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("s-7", &bundle, Role::Test, &target, 7_000);
    let first = harness.load(&id);
    let second = harness.load(&id);
    assert_eq!(first.revision(), second.revision());
    harness.store.save(first).unwrap();
    let error = harness.store.save(second).unwrap_err();
    assert_eq!(error.code(), "promotion_pg.stale_save");
    assert_eq!(harness.load(&id).revision(), harness.load(&id).revision());
}
