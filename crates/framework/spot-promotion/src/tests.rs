//! The pipeline's tests: the acceptance property (a failed rehearsal blocks promotion) and the
//! four failure edges the brief names.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use spot_custom_bundle::{
    ApplyPlan, Bundle, CaptureOptions, FakeSite, Limits, SectionRegistry, capture_with, plan_apply,
};
use spot_environments::{EnvRef, Role};
use spot_migration_approval::{
    Act, Approval, ApprovalError, ApprovalStore, AuditSink, Identity, InMemoryApprovalStore,
    MemoryAuditSink, PlanDigest,
};
use spot_migration_audit::RunId;
use spot_rehearsal::{
    ApplyOutcome, CleanupOutcome, CloneRecord, InvariantReport, InvariantSetResult,
    InvariantViolation, RehearsalOutcome, RehearsalReport, ReplayDecision, ReplayPolicy,
};
use spot_tenancy::TenantId;

use crate::{
    ApplyEvidence, BundleApplier, CustomBundleApplier, FailureEvidence, MemoryPromotionStore,
    OpenRun, PlanIssue, PromotionAudit, PromotionError, PromotionPolicy, PromotionService,
    PromotionState, PromotionStore, RehearsalRequest, RehearsalRunner, RestoreEvidence, TargetSite,
    VerifyEvidence,
};

/// The audit events one test execution produced.
type Events = Arc<Mutex<Vec<String>>>;

/// A recording audit port: every event names the promotion it belongs to.
#[derive(Default)]
struct RecordingAudit {
    events: Events,
}

impl RecordingAudit {
    fn entries(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl PromotionAudit for RecordingAudit {
    fn open<'a>(&'a self, request: OpenRun<'a>) -> crate::ports::PromotionFuture<'a> {
        self.events.lock().unwrap().push(format!(
            "{}:open:{}:{}",
            request.promotion, request.run, request.plan.summary.changes
        ));
        Box::pin(async { Ok(()) })
    }

    fn approval<'a>(
        &'a self,
        promotion: &'a RunId,
        event: spot_migration_audit::ApprovalEvent,
    ) -> crate::ports::PromotionFuture<'a> {
        self.events.lock().unwrap().push(format!(
            "{promotion}:approval:{}:{}",
            event.approver,
            event.decision.as_str()
        ));
        Box::pin(async { Ok(()) })
    }

    fn rehearsal<'a>(
        &'a self,
        promotion: &'a RunId,
        event: spot_migration_audit::RehearsalEvent,
    ) -> crate::ports::PromotionFuture<'a> {
        self.events.lock().unwrap().push(format!(
            "{promotion}:rehearsal:{}:{}",
            event.verdict.as_str(),
            event.findings
        ));
        Box::pin(async { Ok(()) })
    }

    fn applied<'a>(
        &'a self,
        promotion: &'a RunId,
        event: spot_migration_audit::ApplyEvent,
    ) -> crate::ports::PromotionFuture<'a> {
        self.events
            .lock()
            .unwrap()
            .push(format!("{promotion}:applied:{}", event.steps));
        Box::pin(async { Ok(()) })
    }

    fn finish<'a>(
        &'a self,
        promotion: &'a RunId,
        event: spot_migration_audit::FinishEvent,
    ) -> crate::ports::PromotionFuture<'a> {
        self.events
            .lock()
            .unwrap()
            .push(format!("{promotion}:finish:{}", event.outcome.as_str()));
        Box::pin(async { Ok(()) })
    }
}

/// The bundle applier with a switch for the verification failure and call counters.
struct TestApplier {
    inner: CustomBundleApplier,
    fail_verify: bool,
    applies: AtomicUsize,
    verifies: AtomicUsize,
}

impl TestApplier {
    fn new(fail_verify: bool) -> Self {
        Self {
            inner: CustomBundleApplier::with_builtin_sections(),
            fail_verify,
            applies: AtomicUsize::new(0),
            verifies: AtomicUsize::new(0),
        }
    }

    fn verifies(&self) -> usize {
        self.verifies.load(Ordering::SeqCst)
    }

    fn applies(&self) -> usize {
        self.applies.load(Ordering::SeqCst)
    }
}

impl BundleApplier for TestApplier {
    fn plan(&self, bundle: &Bundle, target: &dyn TargetSite) -> Result<ApplyPlan, PromotionError> {
        self.inner.plan(bundle, target)
    }

    fn capture(&self, target: &dyn TargetSite) -> Result<Bundle, PromotionError> {
        self.inner.capture(target)
    }

    fn apply(
        &self,
        plan: &ApplyPlan,
        target: &mut dyn TargetSite,
    ) -> Result<ApplyEvidence, PromotionError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        self.inner.apply(plan, target)
    }

    fn verify(
        &self,
        plan: &ApplyPlan,
        bundle: &Bundle,
        target: &dyn TargetSite,
    ) -> Result<VerifyEvidence, PromotionError> {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        if self.fail_verify {
            return Ok(VerifyEvidence {
                passed: false,
                findings: vec!["scripts/party_guard.ts: the item did not apply".to_owned()],
                replanned_hash: String::new(),
            });
        }
        self.inner.verify(plan, bundle, target)
    }

    fn restore(
        &self,
        pre_image: &Bundle,
        target: &mut dyn TargetSite,
    ) -> Result<RestoreEvidence, PromotionError> {
        self.inner.restore(pre_image, target)
    }
}

/// A rehearsal runner that returns a fixed verdict.
struct FixedRunner {
    outcome: RehearsalOutcome,
}

impl RehearsalRunner for FixedRunner {
    fn rehearse(&self, request: &RehearsalRequest<'_>) -> Result<RehearsalReport, PromotionError> {
        // The ticket the service passes must be the real one for exactly this plan.
        assert!(request.ticket.matches(request.plan.release()));
        Ok(report(request.plan, self.outcome))
    }
}

/// A report for `plan` with `outcome` and, for a failing verdict, one invariant violation.
fn report(plan: &crate::PromotionPlan, outcome: RehearsalOutcome) -> RehearsalReport {
    let violations = if outcome == RehearsalOutcome::InvariantsFailed {
        vec![InvariantViolation::new(
            "l7",
            "trial_balance",
            "account",
            "a-1",
            "mismatch",
        )]
    } else {
        Vec::new()
    };
    RehearsalReport {
        ticket: "ticket-1".to_owned(),
        site: "scratch".to_owned(),
        plan_digest: plan.release().plan_digest.clone(),
        clone: CloneRecord {
            site: "clone".to_owned(),
            label: "scratch-db".to_owned(),
            snapshot_id: "snap-1".to_owned(),
            cleanup: CleanupOutcome::Cleaned,
        },
        pre_digest: "pre".to_owned(),
        post_digest: "post".to_owned(),
        invariants: InvariantReport {
            sets: vec![InvariantSetResult {
                set: "l7".to_owned(),
                passed: violations.is_empty(),
                violations,
            }],
        },
        apply: ApplyOutcome {
            executed: vec!["apply".to_owned()],
            plan_hash: plan.apply_hash().to_owned(),
            plan_digest: plan.release().plan_digest.clone(),
            backfilled_rows: 0,
        },
        diffs: Vec::new(),
        replay: ReplayDecision {
            policy: ReplayPolicy::Off,
            replay: false,
            reason: "the site's replay policy is off".to_owned(),
            triggers: Vec::new(),
        },
        outcome,
    }
}

/// The built-in section registry.
fn registry() -> SectionRegistry {
    SectionRegistry::new()
}

/// A site with an app and one script: the source of the bundle.
fn source_site() -> FakeSite {
    FakeSite::new()
        .with_app("base", semver::Version::new(1, 0, 0))
        .with_script(
            "party_guard",
            "base",
            "party.before_save",
            "export function run(doc: any) { return doc; }",
        )
}

/// A target with the same app and no script: the script is an additive change.
fn target_site(role: Role) -> EnvSite {
    EnvSite::new(
        FakeSite::new().with_app("base", semver::Version::new(1, 0, 0)),
        env(role, 2),
    )
}

/// Captures `site` into a bundle.
fn capture(site: &FakeSite) -> Bundle {
    capture_with(
        site,
        &CaptureOptions::new("site", "1.0.0", "user:req", "2026-10-20T00:00:00Z"),
        &registry(),
        &Limits::default(),
    )
    .unwrap()
}

/// An environment address in set `acme`.
fn env(role: Role, seed: u64) -> EnvRef {
    EnvRef {
        set: "acme".to_owned(),
        role,
        tenant: TenantId::new_v7(seed, [seed as u8; 10]),
    }
}

/// A fake site that belongs to one environment and counts every time the reader or writer port is
/// taken from it, so a test can prove a refused promotion never touched the site.
struct EnvSite {
    site: FakeSite,
    env: EnvRef,
    touches: AtomicUsize,
}

impl EnvSite {
    fn new(site: FakeSite, env: EnvRef) -> Self {
        Self {
            site,
            env,
            touches: AtomicUsize::new(0),
        }
    }

    /// How many reader or writer calls reached the site.
    fn touches(&self) -> usize {
        self.touches.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        self.touches.fetch_add(1, Ordering::SeqCst);
    }
}

impl Deref for EnvSite {
    type Target = FakeSite;
    fn deref(&self) -> &FakeSite {
        &self.site
    }
}

impl DerefMut for EnvSite {
    fn deref_mut(&mut self) -> &mut FakeSite {
        &mut self.site
    }
}

impl TargetSite for EnvSite {
    fn environment(&self) -> EnvRef {
        self.env.clone()
    }
    fn as_reader(&self) -> &dyn spot_custom_bundle::SiteReader {
        self.touch();
        &self.site
    }
    fn as_writer(&mut self) -> &mut dyn spot_custom_bundle::SiteWriter {
        self.touch();
        &mut self.site
    }
}

/// One service wired to recording doubles.
struct Harness {
    service: PromotionService,
    store: Arc<MemoryPromotionStore>,
    approvals: Arc<InMemoryApprovalStore>,
    approval_audit: Arc<MemoryAuditSink>,
    audit: Arc<RecordingAudit>,
    applier: Arc<TestApplier>,
}

impl Harness {
    fn new(policy: PromotionPolicy, fail_verify: bool) -> Harness {
        let store = Arc::new(MemoryPromotionStore::new());
        let approvals = Arc::new(InMemoryApprovalStore::new());
        let approval_audit = Arc::new(MemoryAuditSink::new());
        let audit = Arc::new(RecordingAudit::default());
        let applier = Arc::new(TestApplier::new(fail_verify));
        let service = PromotionService::new(
            store.clone(),
            approvals.clone(),
            approval_audit.clone(),
            audit.clone(),
            applier.clone(),
            policy,
        )
        .unwrap();
        Harness {
            service,
            store,
            approvals,
            approval_audit,
            audit,
            applier,
        }
    }

    /// A service over the same stores but a fresh audit adapter: a restarted process.
    fn restart(&self, policy: PromotionPolicy) -> Harness {
        let audit = Arc::new(RecordingAudit::default());
        let applier = Arc::new(TestApplier::new(false));
        let service = PromotionService::new(
            self.store.clone(),
            self.approvals.clone(),
            self.approval_audit.clone(),
            audit.clone(),
            applier.clone(),
            policy,
        )
        .unwrap();
        Harness {
            service,
            store: self.store.clone(),
            approvals: self.approvals.clone(),
            approval_audit: self.approval_audit.clone(),
            audit,
            applier,
        }
    }

    /// Requests and plans `bundle` to `to`, from the environment one role below it.
    fn planned(
        &self,
        id: &str,
        bundle: &Bundle,
        to: Role,
        target: &dyn TargetSite,
        at: i64,
    ) -> (RunId, crate::Promotion) {
        let run = RunId::new(id).unwrap();
        let from = match to {
            Role::Test => Role::Dev,
            Role::Prod => Role::Test,
            Role::Dev => panic!("a promotion never targets development"),
        };
        self.service
            .request(id, bundle, env(from, 1), env(to, 2), "user:req", at)
            .unwrap();
        let promotion =
            block_on(
                self.service
                    .plan(&run, bundle, target, Vec::new(), "user:req", at + 1),
            )
            .unwrap();
        (run, promotion)
    }

    fn load(&self, id: &RunId) -> crate::Promotion {
        self.store.load(id).unwrap().unwrap()
    }
}

/// One act by `who`, who holds exactly `role` and claims it.
fn act<'a>(who: &str, role: &'a str, now: i64) -> Act<'a> {
    act_as(Identity::new(who, &[role]), role, now)
}

/// One act of `identity` in the claimed `role`.
fn act_as(identity: Identity, role: &str, now: i64) -> Act<'_> {
    Act {
        identity,
        role,
        comment: "looks right",
        signature: None,
        now,
    }
}

/// A promotion policy whose production environment needs the default two owners.
fn prod_policy() -> PromotionPolicy {
    PromotionPolicy::default()
}

#[test]
fn failed_rehearsal_blocks_promotion_and_leaves_the_target_untouched() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let before = capture(&target).digest();
    let bundle = capture(&source_site());
    let (id, promotion) = harness.planned("p-1", &bundle, Role::Prod, &target, 1_000);
    assert_eq!(promotion.state(), PromotionState::Planned);
    block_on(
        harness
            .service
            .approve(&id, &act("user:ana", "owner", 1_100)),
    )
    .unwrap();
    let approved = block_on(
        harness
            .service
            .approve(&id, &act("user:ben", "owner", 1_200)),
    )
    .unwrap();
    assert_eq!(approved.state(), PromotionState::Approved);

    let runner = FixedRunner {
        outcome: RehearsalOutcome::InvariantsFailed,
    };
    let error = block_on(
        harness
            .service
            .rehearse(&id, &bundle, &target, &runner, "user:req", 1_300),
    )
    .unwrap_err();
    assert_eq!(error.code(), "promotion.rehearsal_failed");

    let failed = harness.load(&id);
    assert_eq!(failed.state(), PromotionState::Failed);
    assert!(
        failed
            .journal()
            .iter()
            .all(|transition| transition.to != PromotionState::Applied),
        "the promotion never reached applied"
    );
    assert_eq!(
        failed.failure().map(|evidence| evidence.stage.as_str()),
        Some("rehearsal")
    );
    assert_eq!(
        failed.rehearsal().map(crate::RehearsalEvidence::verdict),
        Some("invariants_failed")
    );
    assert_eq!(capture(&target).digest(), before);
    assert_eq!(harness.applier.applies(), 0);

    let refused = block_on(harness.service.apply(
        &id,
        &bundle,
        &mut target_site(Role::Prod),
        "user:req",
        1_400,
    ))
    .unwrap_err();
    assert_eq!(refused.code(), "promotion.bad_state");
    assert!(
        harness
            .audit
            .entries()
            .iter()
            .any(|event| event == "p-1:finish:failed")
    );
}

#[test]
fn a_plan_that_changed_after_approval_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, promotion) = harness.planned("p-2", &bundle, Role::Test, &target, 2_000);
    assert_eq!(promotion.state(), PromotionState::Approved);
    let approved_hash = promotion.plan().unwrap().apply_hash().to_owned();

    // Someone changes the target between approval and apply: the bundle now plans a removal the
    // approval never covered.
    target.site = target.site.clone().with_script(
        "nightly_rollup",
        "base",
        "party.before_save",
        "export function run(doc: any) { return doc; }",
    );
    let current = plan_apply(&bundle, &target.site, &registry()).unwrap();
    assert_ne!(current.hash(), approved_hash);

    let error = block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 2_100),
    )
    .unwrap_err();
    assert_eq!(error.code(), "promotion.plan_changed");
    assert_eq!(harness.applier.applies(), 0);
    assert_eq!(harness.load(&id).state(), PromotionState::Approved);
}

#[test]
fn prod_needs_two_owner_approvals_and_the_requester_never_counts() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, promotion) = harness.planned("p-3", &bundle, Role::Prod, &target, 3_000);
    assert_eq!(promotion.state(), PromotionState::Planned);

    // The requester and a viewer are refused outright; neither is recorded.
    let by_requester = block_on(
        harness
            .service
            .approve(&id, &act("user:req", "owner", 3_100)),
    )
    .unwrap_err();
    assert_eq!(by_requester.code(), "promotion.approval_refused");
    let wrong_role = block_on(
        harness
            .service
            .approve(&id, &act("user:cat", "viewer", 3_200)),
    )
    .unwrap_err();
    assert_eq!(wrong_role.code(), "promotion.approval_refused");
    assert!(harness.approvals.all().unwrap().is_empty());
    assert_eq!(harness.load(&id).state(), PromotionState::Planned);

    let one_owner = block_on(
        harness
            .service
            .approve(&id, &act("user:ana", "owner", 3_300)),
    )
    .unwrap();
    assert_eq!(one_owner.state(), PromotionState::Planned);

    let two_owners = block_on(
        harness
            .service
            .approve(&id, &act("user:ben", "owner", 3_400)),
    )
    .unwrap();
    assert_eq!(two_owners.state(), PromotionState::Approved);
    assert!(
        harness
            .audit
            .entries()
            .iter()
            .filter(|event| event.starts_with("p-3:approval:"))
            .count()
            == 2
    );
}

#[test]
fn test_environment_auto_approves_an_additive_bundle() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (_id, promotion) = harness.planned("p-4", &bundle, Role::Test, &target, 4_000);
    assert_eq!(promotion.state(), PromotionState::Approved);
    assert!(
        harness
            .audit
            .entries()
            .iter()
            .any(|event| event == "p-4:approval:system:approved"),
        "the auto-approval is recorded"
    );
}

#[test]
fn verify_failure_restores_the_target() {
    let harness = Harness::new(PromotionPolicy::default(), true);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let before = capture(&target).digest();
    let (id, promotion) = harness.planned("p-5", &bundle, Role::Test, &target, 5_000);
    assert_eq!(promotion.state(), PromotionState::Approved);

    let applied = block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 5_100),
    )
    .unwrap();
    assert_eq!(applied.state(), PromotionState::Applied);
    assert_ne!(capture(&target).digest(), before);

    let error = block_on(
        harness
            .service
            .verify(&id, &bundle, &mut target, "user:req", 5_200),
    )
    .unwrap_err();
    assert_eq!(error.code(), "promotion.verify_failed");
    let failed = harness.load(&id);
    assert_eq!(failed.state(), PromotionState::Failed);
    let failure: &FailureEvidence = failed.failure().unwrap();
    assert_eq!(failure.stage, "verify");
    assert_eq!(failure.findings.len(), 1);
    assert_eq!(
        capture(&target).digest(),
        before,
        "the target is back to its pre-state"
    );
    assert!(
        harness
            .audit
            .entries()
            .iter()
            .any(|event| event == "p-5:finish:failed")
    );
}

#[test]
fn resume_after_a_kill_verifies_instead_of_applying_twice() {
    let policy = PromotionPolicy::default();
    let harness = Harness::new(policy.clone(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _promotion) = harness.planned("p-6", &bundle, Role::Test, &target, 6_000);
    block_on(
        harness
            .service
            .apply(&id, &bundle, &mut target, "user:req", 6_100),
    )
    .unwrap();
    assert_eq!(harness.load(&id).state(), PromotionState::Applied);

    // The process dies here. A new service reads the same store and reattaches.
    let restarted = harness.restart(policy);
    let resumed = block_on(restarted.service.resume(&id, "user:req", 6_200)).unwrap();
    assert_eq!(resumed.state(), PromotionState::Applied);
    assert_eq!(resumed.executions(), 1);
    assert!(
        restarted
            .audit
            .entries()
            .iter()
            .any(|event| event == "p-6:open:p-6:resume-1:1"),
        "the resumed execution opens a derived run id"
    );

    let verified = block_on(
        restarted
            .service
            .verify(&id, &bundle, &mut target, "user:req", 6_300),
    )
    .unwrap();
    assert_eq!(verified.state(), PromotionState::Verified);
    assert_eq!(restarted.applier.applies(), 0, "verification never applies");
    assert_eq!(
        restarted.load(&id).journal().last().map(|t| t.to),
        Some(PromotionState::Verified)
    );
}

#[test]
fn a_blocking_issue_stops_planning_before_anything_is_written() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let before = capture(&target).digest();
    let bundle = capture(&source_site());
    let id = RunId::new("p-7").unwrap();
    harness
        .service
        .request(
            "p-7",
            &bundle,
            env(Role::Dev, 1),
            env(Role::Test, 2),
            "user:req",
            7_000,
        )
        .unwrap();
    let issue = PlanIssue {
        source: crate::IssueSource::Merge,
        code: "bundle_merge.conflict_both_modified".to_owned(),
        item: "scripts/party_guard".to_owned(),
        message: "both sides changed the script".to_owned(),
        risk: spot_migration_plan::Risk::Destructive,
        blocking: true,
    };
    let error =
        block_on(
            harness
                .service
                .plan(&id, &bundle, &target, vec![issue], "user:req", 7_100),
        )
        .unwrap_err();
    assert_eq!(error.code(), "promotion.blocked");
    assert_eq!(harness.load(&id).state(), PromotionState::Requested);
    assert_eq!(harness.applier.applies(), 0);
    assert_eq!(capture(&target).digest(), before);
}

#[test]
fn a_rejection_is_terminal_and_blocks_apply() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, promotion) = harness.planned("p-8", &bundle, Role::Prod, &target, 8_000);
    assert_eq!(promotion.state(), PromotionState::Planned);
    let rejected = block_on(
        harness
            .service
            .reject(&id, &act("user:ana", "owner", 8_100)),
    )
    .unwrap();
    assert_eq!(rejected.state(), PromotionState::Rejected);

    let error = block_on(harness.service.apply(
        &id,
        &bundle,
        &mut target_site(Role::Prod),
        "user:req",
        8_200,
    ))
    .unwrap_err();
    assert_eq!(error.code(), "promotion.bad_state");
}

#[test]
fn the_plan_digest_binds_the_exact_changes() {
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let first = plan_apply(&bundle, &target.site, &registry()).unwrap();
    let second = plan_apply(&bundle, &target.site, &registry()).unwrap();
    assert_eq!(first.hash(), second.hash());

    let harness = Harness::new(PromotionPolicy::default(), false);
    let (id, promotion) = harness.planned("p-9", &bundle, Role::Test, &target, 9_000);
    let plan = promotion.plan().unwrap();
    assert!(plan.digest().as_hex().len() == 64);
    assert!(plan.release().plan_digest.len() == 64);
    assert_eq!(plan.apply_changes(), 1);

    // A different bundle (one more script) is a different plan digest.
    let other_bundle = capture(
        &target_site(Role::Test)
            .site
            .with_script(
                "party_guard",
                "base",
                "party.before_save",
                "export function run(doc: any) { return doc; }",
            )
            .with_script(
                "nightly_rollup",
                "base",
                "party.before_save",
                "export function run(doc: any) { return doc; }",
            ),
    );
    let other = Harness::new(PromotionPolicy::default(), false);
    let (_other_id, other_promotion) =
        other.planned("p-10", &other_bundle, Role::Test, &target, 9_100);
    assert_ne!(plan.digest(), other_promotion.plan().unwrap().digest());
    assert_eq!(&id, promotion.id());
}

#[test]
fn every_transition_is_journaled_and_audited_under_the_promotion_id() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let mut target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (id, _promotion) = harness.planned("p-11", &bundle, Role::Test, &target, 11_000);
    let verified = block_on(
        harness
            .service
            .advance(&id, &bundle, &mut target, "user:req", 11_100),
    )
    .unwrap();
    assert_eq!(verified.state(), PromotionState::Verified);

    let states: Vec<PromotionState> = verified
        .journal()
        .iter()
        .map(|transition| transition.to)
        .collect();
    assert_eq!(
        states,
        vec![
            PromotionState::Requested,
            PromotionState::Planned,
            PromotionState::Approved,
            PromotionState::Applied,
            PromotionState::Verified,
        ]
    );
    let events = harness.audit.entries();
    for expected in ["open", "approval", "applied", "finish"] {
        assert!(
            events
                .iter()
                .any(|event| event.starts_with("p-11:") && event.contains(expected)),
            "the audit records the {expected} event"
        );
    }
    assert!(events.iter().all(|event| event.starts_with("p-11:")));
}

#[test]
fn a_route_that_skips_an_environment_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let bundle = capture(&source_site());
    let error = harness
        .service
        .request(
            "p-12",
            &bundle,
            env(Role::Dev, 1),
            env(Role::Prod, 2),
            "user:req",
            12_000,
        )
        .unwrap_err();
    assert_eq!(error.code(), "promotion.bad_route");
}

#[test]
fn a_bundle_that_is_not_the_requested_one_is_refused() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let (_id, _promotion) = harness.planned("p-13", &bundle, Role::Test, &target, 13_000);
    let other = capture(&target_site(Role::Test).site.with_script(
        "other",
        "base",
        "party.before_save",
        "export function run(doc: any) { return doc; }",
    ));
    let id = RunId::new("p-13").unwrap();
    let error = block_on(harness.service.apply(
        &id,
        &other,
        &mut target_site(Role::Test),
        "user:req",
        13_100,
    ))
    .unwrap_err();
    assert_eq!(error.code(), "promotion.bundle_mismatch");
}

#[test]
fn an_invalid_policy_is_refused_at_construction() {
    let policy = PromotionPolicy {
        expiry_micros: 0,
        ..PromotionPolicy::default()
    };
    let store = Arc::new(MemoryPromotionStore::new());
    let approvals = Arc::new(InMemoryApprovalStore::new());
    let audit = Arc::new(RecordingAudit::default());
    let applier = Arc::new(TestApplier::new(false));
    let error = match PromotionService::new(
        store,
        approvals,
        Arc::new(MemoryAuditSink::new()),
        audit,
        applier,
        policy,
    ) {
        Ok(_) => panic!("a policy with no approval expiry must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "promotion.policy_invalid");
}

#[test]
fn release_conflicts_become_blocking_or_warning_plan_items() {
    use spot_migration_conflicts::{
        ArtefactKind, ArtefactRef, Cause, ConflictCategory, ConflictItem, ConflictReport,
    };
    use spot_migration_plan::Risk;

    let item = |category: ConflictCategory, code: &'static str, risk: Risk| ConflictItem {
        id: code.to_owned(),
        code,
        category,
        cause: Cause::new(
            "metadata/party/field_removed",
            "a field the site uses is removed",
        ),
        artefacts: vec![ArtefactRef::new(ArtefactKind::Script, "script:party_guard")],
        risk,
        resolution: "Update the script to the new field.".to_owned(),
    };
    let report = ConflictReport {
        plan_digest: "digest".to_owned(),
        items: vec![
            item(
                ConflictCategory::ScriptTypeError,
                "migration_conflicts.script_type_error",
                Risk::Destructive,
            ),
            item(
                ConflictCategory::OverrideOptionsConflict,
                "migration_conflicts.override_options_conflict",
                Risk::NeedsReview,
            ),
        ],
    };
    let issues = PlanIssue::from_upgrade(&report);
    assert_eq!(issues.len(), 2);
    assert!(
        issues[0].blocking,
        "a destructive conflict blocks promotion"
    );
    assert_eq!(issues[0].source, crate::IssueSource::Upgrade);
    assert_eq!(issues[0].item, "script:script:party_guard");
    assert!(!issues[1].blocking, "a review-only conflict is a warning");
}

#[test]
fn a_warning_conflict_is_part_of_the_approved_plan() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Test);
    let bundle = capture(&source_site());
    let without = Harness::new(PromotionPolicy::default(), false);
    let (_a, plain) = without.planned("p-14a", &bundle, Role::Test, &target, 14_000);

    let id = RunId::new("p-14b").unwrap();
    harness
        .service
        .request(
            "p-14b",
            &bundle,
            env(Role::Dev, 1),
            env(Role::Test, 2),
            "user:req",
            14_100,
        )
        .unwrap();
    let warning = PlanIssue {
        source: crate::IssueSource::Upgrade,
        code: "migration_conflicts.override_options_conflict".to_owned(),
        item: "override:party.status".to_owned(),
        message: "the option list changed".to_owned(),
        risk: spot_migration_plan::Risk::NeedsReview,
        blocking: false,
    };
    let promotion =
        block_on(
            harness
                .service
                .plan(&id, &bundle, &target, vec![warning], "user:req", 14_200),
        )
        .unwrap();
    // The warning raises the plan's risk class, so even test needs one approver for it.
    assert_eq!(promotion.state(), PromotionState::Planned);
    let approved = block_on(
        harness
            .service
            .approve(&id, &act("user:ana", "release_manager", 14_300)),
    )
    .unwrap();
    assert_eq!(approved.state(), PromotionState::Approved);
    let plan = approved.plan().unwrap();
    assert_eq!(plan.issues().len(), 1);
    assert!(
        plan.release()
            .changes
            .iter()
            .any(|change| change.id.starts_with("conflict/")),
        "the warning is in the plan document a reviewer approves"
    );
    assert_ne!(
        plan.digest(),
        plain.plan().unwrap().digest(),
        "the warning changes what was approved"
    );
}

#[test]
fn an_approval_for_one_promotion_is_not_valid_for_another_of_the_same_bundle() {
    let harness = Harness::new(PromotionPolicy::default(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (first_id, first) = harness.planned("p-replay-1", &bundle, Role::Prod, &target, 20_000);
    let (second_id, second) = harness.planned("p-replay-2", &bundle, Role::Prod, &target, 20_100);
    let first_digest = first.plan().unwrap().digest().clone();
    assert_ne!(first_digest, *second.plan().unwrap().digest());

    // Both owners approve the first promotion: the second stays unapproved, because an approval
    // is recorded against the plan of the promotion it was made on.
    for (who, at) in [("user:ana", 20_200), ("user:ben", 20_300)] {
        block_on(harness.service.approve(&first_id, &act(who, "owner", at))).unwrap();
    }
    assert_eq!(harness.load(&first_id).state(), PromotionState::Approved);
    assert_eq!(harness.load(&second_id).state(), PromotionState::Planned);
    assert!(
        harness
            .approvals
            .for_digest(second.plan().unwrap().digest())
            .unwrap()
            .is_empty()
    );
}

/// A site in `set` with `role` and tenant seed `seed`, holding the same state as [`target_site`].
fn site_in(set: &str, role: Role, seed: u64) -> EnvSite {
    let mut env = env(role, seed);
    env.set = set.to_owned();
    EnvSite::new(
        FakeSite::new().with_app("base", semver::Version::new(1, 0, 0)),
        env,
    )
}

/// Sites that are not the test environment `acme/test/tenant-2`: every kind of difference,
/// including a missing (empty) set name and case or whitespace variants of the right one.
fn wrong_sites() -> Vec<(&'static str, EnvSite)> {
    vec![
        ("other role", site_in("acme", Role::Prod, 2)),
        ("other tenant", site_in("acme", Role::Test, 3)),
        ("other set", site_in("beta", Role::Test, 2)),
        ("set in upper case", site_in("ACME", Role::Test, 2)),
        ("set with leading space", site_in(" acme", Role::Test, 2)),
        ("set with trailing space", site_in("acme ", Role::Test, 2)),
        (
            "set with a trailing newline",
            site_in("acme\n", Role::Test, 2),
        ),
        ("missing (empty) set", site_in("", Role::Test, 2)),
    ]
}

/// The mismatch error names only environments, never anything read from a site or a bundle.
fn assert_mismatch(error: &PromotionError) {
    assert_eq!(error.code(), "promotion.environment_mismatch");
    let message = error.to_string();
    assert!(message.contains("promotion.environment_mismatch"));
    assert!(!message.contains("export function"), "no script source");
    assert!(!message.contains("party_guard"), "no item names");
}

#[test]
fn plan_refuses_a_site_of_another_environment_before_reading_it() {
    let bundle = capture(&source_site());
    for (name, site) in wrong_sites() {
        let harness = Harness::new(PromotionPolicy::default(), false);
        let id = RunId::new("p-env-plan").unwrap();
        harness
            .service
            .request(
                "p-env-plan",
                &bundle,
                env(Role::Dev, 1),
                env(Role::Test, 2),
                "user:req",
                30_000,
            )
            .unwrap();
        let error =
            block_on(
                harness
                    .service
                    .plan(&id, &bundle, &site, Vec::new(), "user:req", 30_100),
            )
            .unwrap_err();
        assert_mismatch(&error);
        assert_eq!(site.touches(), 0, "{name}: the site was read or written");
        assert_eq!(
            harness.load(&id).state(),
            PromotionState::Requested,
            "{name}"
        );
        assert!(harness.load(&id).plan().is_none(), "{name}: no plan stored");
        assert_eq!(harness.applier.applies(), 0, "{name}");
    }
}

#[test]
fn apply_refuses_a_site_of_another_environment_and_leaves_it_untouched() {
    let bundle = capture(&source_site());
    for (name, mut site) in wrong_sites() {
        let harness = Harness::new(PromotionPolicy::default(), false);
        let right = target_site(Role::Test);
        let (id, promotion) = harness.planned("p-env-apply", &bundle, Role::Test, &right, 31_000);
        assert_eq!(promotion.state(), PromotionState::Approved);
        let before = capture(&site).digest();
        let error = block_on(
            harness
                .service
                .apply(&id, &bundle, &mut site, "user:req", 31_100),
        )
        .unwrap_err();
        assert_mismatch(&error);
        assert_eq!(capture(&site).digest(), before, "{name}: site changed");
        assert_eq!(
            site.touches(),
            0,
            "{name}: the site was read or written after the refusal"
        );
        assert_eq!(harness.applier.applies(), 0, "{name}");
        assert_eq!(
            harness.load(&id).state(),
            PromotionState::Approved,
            "{name}"
        );
    }
}

#[test]
fn verify_refuses_a_site_of_another_environment() {
    let bundle = capture(&source_site());
    for (name, mut site) in wrong_sites() {
        let harness = Harness::new(PromotionPolicy::default(), false);
        let mut right = target_site(Role::Test);
        let (id, _) = harness.planned("p-env-verify", &bundle, Role::Test, &right, 32_000);
        block_on(
            harness
                .service
                .apply(&id, &bundle, &mut right, "user:req", 32_100),
        )
        .unwrap();
        assert_eq!(harness.load(&id).state(), PromotionState::Applied);
        let error = block_on(
            harness
                .service
                .verify(&id, &bundle, &mut site, "user:req", 32_200),
        )
        .unwrap_err();
        assert_mismatch(&error);
        assert_eq!(site.touches(), 0, "{name}: the site was read or written");
        assert_eq!(harness.applier.verifies(), 0, "{name}");
        assert_eq!(harness.load(&id).state(), PromotionState::Applied, "{name}");
    }
}

#[test]
fn rehearse_refuses_a_site_of_another_environment() {
    let bundle = capture(&source_site());
    let right = target_site(Role::Prod);
    // A production promotion: the rehearsal must not read a test or other site in its place.
    for (name, site) in [
        ("test site", site_in("acme", Role::Test, 2)),
        ("other tenant", site_in("acme", Role::Prod, 3)),
        ("upper-case set", site_in("ACME", Role::Prod, 2)),
        ("missing set", site_in("", Role::Prod, 2)),
    ] {
        let harness = Harness::new(prod_policy(), false);
        let (id, _) = harness.planned("p-env-rehearse", &bundle, Role::Prod, &right, 33_000);
        for (who, at) in [("user:ana", 33_100), ("user:ben", 33_200)] {
            block_on(harness.service.approve(&id, &act(who, "owner", at))).unwrap();
        }
        let runner = FixedRunner {
            outcome: RehearsalOutcome::Clean,
        };
        let error = block_on(
            harness
                .service
                .rehearse(&id, &bundle, &site, &runner, "user:req", 33_300),
        )
        .unwrap_err();
        assert_mismatch(&error);
        assert_eq!(site.touches(), 0, "{name}");
        assert_eq!(
            harness.load(&id).state(),
            PromotionState::Approved,
            "{name}"
        );
        assert!(harness.load(&id).rehearsal().is_none(), "{name}");
    }
}

#[test]
fn the_plan_digest_differs_when_only_the_environment_differs() {
    let bundle = capture(&source_site());
    let digest_of = |set: &str, from_seed: u64, to_seed: u64, to: Role| {
        let harness = Harness::new(PromotionPolicy::default(), false);
        let from_role = if to == Role::Test {
            Role::Dev
        } else {
            Role::Test
        };
        let mut from = env(from_role, from_seed);
        from.set = set.to_owned();
        let mut to_env = env(to, to_seed);
        to_env.set = set.to_owned();
        let site = EnvSite::new(
            FakeSite::new().with_app("base", semver::Version::new(1, 0, 0)),
            to_env.clone(),
        );
        harness
            .service
            .request("p-hash", &bundle, from, to_env, "user:req", 34_000)
            .unwrap();
        let id = RunId::new("p-hash").unwrap();
        let promotion =
            block_on(
                harness
                    .service
                    .plan(&id, &bundle, &site, Vec::new(), "user:req", 34_100),
            )
            .unwrap();
        promotion.plan().unwrap().digest().clone()
    };
    let base = digest_of("acme", 1, 2, Role::Test);
    assert_eq!(base, digest_of("acme", 1, 2, Role::Test), "stable");
    assert_ne!(base, digest_of("acme", 1, 3, Role::Test), "target tenant");
    assert_ne!(base, digest_of("acme", 4, 2, Role::Test), "source tenant");
    assert_ne!(base, digest_of("beta", 1, 2, Role::Test), "environment set");
    assert_ne!(base, digest_of("acme", 1, 2, Role::Prod), "target role");
}

/// An approval store that panics on an append no approval-service record precedes: the service
/// audits first, so a direct append by the pipeline has no `migration.approved` or
/// `migration.rejected` record ahead of it.
struct GuardedStore {
    inner: InMemoryApprovalStore,
    audit: Arc<MemoryAuditSink>,
    appended: AtomicUsize,
}

impl ApprovalStore for GuardedStore {
    fn append(&self, approval: Approval) -> Result<(), ApprovalError> {
        let decisions = self
            .audit
            .records()
            .iter()
            .filter(|record| {
                record.kind.ends_with(".approved") || record.kind.ends_with(".rejected")
            })
            .count();
        let appended = self.appended.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(
            decisions >= appended,
            "an approval was appended without passing through the approval service"
        );
        self.inner.append(approval)
    }
    fn for_digest(&self, digest: &PlanDigest) -> Result<Vec<Approval>, ApprovalError> {
        self.inner.for_digest(digest)
    }
    fn all(&self) -> Result<Vec<Approval>, ApprovalError> {
        self.inner.all()
    }
    fn register_plan(&self, digest: &PlanDigest, author: &str) -> Result<(), ApprovalError> {
        self.inner.register_plan(digest, author)
    }
    fn author_of(&self, digest: &PlanDigest) -> Result<Option<String>, ApprovalError> {
        self.inner.author_of(digest)
    }
}

#[test]
fn no_path_appends_to_the_approval_store_except_through_the_service() {
    let approval_audit = Arc::new(MemoryAuditSink::new());
    let guarded = Arc::new(GuardedStore {
        inner: InMemoryApprovalStore::new(),
        audit: approval_audit.clone(),
        appended: AtomicUsize::new(0),
    });
    let service = PromotionService::new(
        Arc::new(MemoryPromotionStore::new()),
        guarded.clone(),
        approval_audit.clone(),
        Arc::new(RecordingAudit::default()),
        Arc::new(TestApplier::new(false)),
        prod_policy(),
    )
    .unwrap();
    let mut target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let id = RunId::new("p-guard").unwrap();
    service
        .request(
            "p-guard",
            &bundle,
            env(Role::Test, 1),
            env(Role::Prod, 2),
            "user:req",
            40_000,
        )
        .unwrap();
    block_on(service.plan(&id, &bundle, &target, Vec::new(), "user:req", 40_100)).unwrap();
    for (who, at) in [("user:ana", 40_200), ("user:ben", 40_300)] {
        block_on(service.approve(&id, &act(who, "owner", at))).unwrap();
    }
    let runner = FixedRunner {
        outcome: RehearsalOutcome::Clean,
    };
    block_on(service.rehearse(&id, &bundle, &target, &runner, "user:req", 40_400)).unwrap();
    block_on(service.apply(&id, &bundle, &mut target, "user:req", 40_500)).unwrap();
    let verified = block_on(service.verify(&id, &bundle, &mut target, "user:req", 40_600)).unwrap();
    assert_eq!(verified.state(), PromotionState::Verified);
    assert_eq!(guarded.appended.load(Ordering::SeqCst), 2);
}

#[test]
fn approving_a_promotion_whose_author_was_never_registered_is_refused() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("p-noauthor", &bundle, Role::Prod, &target, 41_000);
    // The same promotion store over an approval store that never saw the plan registered.
    let other = Harness::new(prod_policy(), false);
    let service = PromotionService::new(
        harness.store.clone(),
        other.approvals.clone(),
        other.approval_audit.clone(),
        other.audit.clone(),
        other.applier.clone(),
        prod_policy(),
    )
    .unwrap();
    let error = block_on(service.approve(&id, &act("user:ana", "owner", 41_100))).unwrap_err();
    assert_eq!(error.code(), "promotion.approval_refused");
    assert!(
        error
            .to_string()
            .contains("migration_approval.author_unknown")
    );
    assert!(other.approvals.all().unwrap().is_empty());
    assert_eq!(harness.load(&id).state(), PromotionState::Planned);
}

#[test]
fn the_requester_cannot_approve_their_own_promotion_even_after_a_restart() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("p-restart", &bundle, Role::Prod, &target, 42_000);
    let author = harness
        .approvals
        .author_of(harness.load(&id).plan().unwrap().digest())
        .unwrap();
    assert_eq!(author.as_deref(), Some("user:req"));

    let restarted = harness.restart(prod_policy());
    let error = block_on(
        restarted
            .service
            .approve(&id, &act("user:req", "owner", 42_100)),
    )
    .unwrap_err();
    assert_eq!(error.code(), "promotion.approval_refused");
    assert!(
        error
            .to_string()
            .contains("migration_approval.not_permitted")
    );
    // Others can still approve through the restarted service.
    block_on(
        restarted
            .service
            .approve(&id, &act("user:ana", "owner", 42_200)),
    )
    .unwrap();
    assert_eq!(restarted.approvals.all().unwrap().len(), 1);
}

#[test]
fn a_claimed_role_the_identity_does_not_hold_is_refused() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("p-role", &bundle, Role::Prod, &target, 43_000);
    let liar = Identity::new("user:cat", &["viewer"]);
    for decide_reject in [false, true] {
        let claim = act_as(liar.clone(), "owner", 43_100);
        let error = if decide_reject {
            block_on(harness.service.reject(&id, &claim))
        } else {
            block_on(harness.service.approve(&id, &claim))
        }
        .unwrap_err();
        assert_eq!(error.code(), "promotion.approval_refused");
        assert!(
            error
                .to_string()
                .contains("migration_approval.role_not_held")
        );
    }
    assert!(harness.approvals.all().unwrap().is_empty());
    assert_eq!(harness.load(&id).state(), PromotionState::Planned);
}

#[test]
fn a_wrong_role_approver_is_refused_and_leaves_no_record() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("p-wrong", &bundle, Role::Prod, &target, 44_000);
    let error = block_on(
        harness
            .service
            .approve(&id, &act("user:cat", "viewer", 44_100)),
    )
    .unwrap_err();
    assert_eq!(error.code(), "promotion.approval_refused");
    assert!(
        error
            .to_string()
            .contains("migration_approval.not_permitted")
    );
    assert!(harness.approvals.all().unwrap().is_empty());
    assert!(
        harness
            .approval_audit
            .records()
            .iter()
            .all(|record| !record.kind.ends_with(".approved")),
        "a refused approval writes no approved record"
    );
}

#[test]
fn an_author_that_cannot_be_registered_fails_the_plan_step_closed() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let id = RunId::new("p-blank").unwrap();
    harness
        .service
        .request(
            "p-blank",
            &bundle,
            env(Role::Test, 1),
            env(Role::Prod, 2),
            "  ",
            45_000,
        )
        .unwrap();
    let error =
        block_on(
            harness
                .service
                .plan(&id, &bundle, &target, Vec::new(), "user:req", 45_100),
        )
        .unwrap_err();
    assert_eq!(error.code(), "promotion.approval_refused");
    assert_eq!(harness.load(&id).state(), PromotionState::Requested);
    assert!(harness.load(&id).plan().is_none());
}

/// An audit sink that refuses every record.
struct DownSink;

impl AuditSink for DownSink {
    fn record(&self, _record: spot_audit_chain::AuditRecord) -> Result<(), ApprovalError> {
        Err(ApprovalError::Backend {
            message: "down".to_owned(),
        })
    }
}

#[test]
fn an_unwritable_approval_audit_records_nothing() {
    let harness = Harness::new(prod_policy(), false);
    let target = target_site(Role::Prod);
    let bundle = capture(&source_site());
    let (id, _) = harness.planned("p-down", &bundle, Role::Prod, &target, 46_000);
    let service = PromotionService::new(
        harness.store.clone(),
        harness.approvals.clone(),
        Arc::new(DownSink),
        harness.audit.clone(),
        harness.applier.clone(),
        prod_policy(),
    )
    .unwrap();
    let error = block_on(service.approve(&id, &act("user:ana", "owner", 46_100))).unwrap_err();
    assert_eq!(error.code(), "promotion.audit_failed");
    assert!(harness.approvals.all().unwrap().is_empty());
}
