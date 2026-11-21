//! The governance wiring (WP-IAM-08.T12): the grant gate allows and refuses, a recorded SoD override
//! covers the same triple afterwards and survives a restart, an approval is decided by the named
//! approver only and grants exactly once, a pending approval survives a restart, and every failure to
//! read the state, start the approval or store a record refuses the grant.
//!
//! The gate runs over fakes for the directory, the launcher and the executor, with the shared in-memory
//! `GovernanceStore` standing in for the durable one: a "restart" is a second `Governance` over the same
//! store. The production directory source and the real process launcher are covered by the bundle's
//! live-site run, not here.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_bpm_model::ProcessVersion;
use spot_cli::services::governance::{
    Directory, DirectorySource, Governance, GovernanceStore, GrantExecutor, GrantGuard,
    GrantOutcome, LauncherSlot, MemoryGovernanceStore, Parts,
};
use spot_cli::services::governance_approval::{Completion, GrantCompletion, StepUpVerified};
use spot_core::{Clock, ManualClock};
use spot_http::api::service::BoxFuture;
use spot_iam_governance::approval::LaunchReceipt;
use spot_iam_governance::{
    ApprovalPolicy, DelegationPolicy, GovError, GrantRequest, GrantRequestId, Grantor,
    MitigatingControl, OverrideSubmission, ProcessLauncher,
};
use spot_iam_roles::{Assignment, OutsideGuard};
use spot_sod::{RoleCatalogue, RuleSet};
use time::macros::datetime;

const RULES: &str = r#"
[[rules]]
id = "sod.vendor-master.payments"
title = "Maintain suppliers vs approve payments"
description = "Who maintains the supplier master must not approve the payments to those suppliers."
severity = "critical"
scope = "same_scope"
[[rules.sides]]
duties = ["Maintain supplier master"]
[[rules.sides]]
duties = ["Approve payment run"]
"#;

const CATALOGUE: &str = r#"
[[roles]]
name = "Accounts Payable Clerk"
duties = ["Maintain supplier master"]

[[roles]]
name = "Finance Approver"
duties = ["Approve payment run"]

[[roles]]
name = "Treasury Clerk"
duties = ["Maintain bank details"]
"#;

/// A directory the test can change, or break.
#[derive(Default)]
struct FakeDirectory {
    assignments: Mutex<Vec<Assignment>>,
    broken: AtomicBool,
}

impl FakeDirectory {
    fn hold(&self, user: &str, role: &str) {
        let id = format!("{user}-{role}").replace(' ', "-");
        self.assignments
            .lock()
            .unwrap()
            .push(Assignment::new(&id, user, role));
    }
}

impl DirectorySource for FakeDirectory {
    fn read<'a>(&'a self) -> BoxFuture<'a, Option<Directory>> {
        Box::pin(async move {
            if self.broken.load(Ordering::SeqCst) {
                return None;
            }
            Some(Directory {
                outside: OutsideGuard::none(),
                assignments: self.assignments.lock().unwrap().clone(),
            })
        })
    }
}

/// A launcher that counts launches and answers one Case per request id.
#[derive(Default)]
struct FakeLauncher {
    launches: AtomicUsize,
}

impl ProcessLauncher for FakeLauncher {
    fn launch(
        &self,
        _process: &ProcessVersion,
        request: &GrantRequestId,
    ) -> Result<LaunchReceipt, GovError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(LaunchReceipt {
            case_id: format!("case-{}", request.as_str()),
            process_hash: "hash".to_owned(),
        })
    }
}

/// An executor that records the grants it made and can be made to fail.
#[derive(Default)]
struct FakeExecutor {
    attempts: AtomicUsize,
    fail: AtomicBool,
    granted: Mutex<Vec<String>>,
}

impl GrantExecutor for FakeExecutor {
    fn execute<'a>(
        &'a self,
        request: &'a GrantRequest,
        approver: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("governance_host.state_unavailable".to_owned());
            }
            assert_eq!(
                approver, "carol",
                "the grant is made on behalf of the approver"
            );
            // Idempotent by the request's assignment id, as the real executor must be.
            let id = request.id.assignment_id();
            let mut granted = self.granted.lock().unwrap();
            if !granted.contains(&id) {
                granted.push(id);
            }
            Ok(())
        })
    }
}

/// One site's worth of fakes.
struct World {
    directory: Arc<FakeDirectory>,
    store: Arc<MemoryGovernanceStore>,
    launcher: Arc<FakeLauncher>,
    executor: Arc<FakeExecutor>,
    clock: Arc<dyn Clock>,
}

impl World {
    fn new() -> World {
        World {
            directory: Arc::new(FakeDirectory::default()),
            store: Arc::new(MemoryGovernanceStore::default()),
            launcher: Arc::new(FakeLauncher::default()),
            executor: Arc::new(FakeExecutor::default()),
            clock: Arc::new(ManualClock::new(datetime!(2026-11-01 09:00 UTC))),
        }
    }

    /// A gate over this world; `with_launcher` sets the approval launcher.
    fn gate(&self, approval: ApprovalPolicy, with_launcher: bool) -> Governance {
        let slot = Arc::new(LauncherSlot::default());
        if with_launcher {
            slot.set(self.launcher.clone());
        }
        let store: Arc<dyn GovernanceStore> = self.store.clone();
        Governance::from_parts(Parts {
            site: "acme".to_owned(),
            source: self.directory.clone(),
            store,
            clock: Arc::clone(&self.clock),
            rules: Some(RuleSet::from_toml(RULES).unwrap()),
            catalogue: Some(RoleCatalogue::from_toml(CATALOGUE).unwrap()),
            delegation: DelegationPolicy::Unrestricted,
            approval,
            launcher: slot,
            broken: false,
        })
    }

    fn completion(&self, gate: Governance) -> GrantCompletion {
        GrantCompletion::new(Arc::new(gate), Some(self.executor.clone()))
    }
}

fn request(id: &str, user: &str, role: &str, grantor: Grantor) -> GrantRequest {
    GrantRequest::new(&GrantRequestId::new(id).unwrap(), user, role, grantor)
}

fn with_override(request: GrantRequest) -> GrantRequest {
    request.with_override(OverrideSubmission::new(
        "month-end close only",
        MitigatingControl::new("C-1", "a second reviewer signs the run"),
    ))
}

fn treasury_needs_approval() -> ApprovalPolicy {
    ApprovalPolicy::none()
        .requiring_role("Treasury Clerk")
        .routed_to("carol", "role:approver", 3_600)
}

fn step_up() -> StepUpVerified {
    StepUpVerified::confirmed_by_caller()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_allows_a_clean_grant_and_refuses_a_conflicting_one() {
    let world = World::new();
    let gate = world.gate(ApprovalPolicy::none(), false);
    let admin = Grantor::plain("admin");
    // Nothing conflicts: allowed.
    let clean = request("r-1", "jane", "Treasury Clerk", admin.clone());
    assert_eq!(gate.decide(clean).await, GrantOutcome::Allowed);
    // jane approves payments; maintaining suppliers as well is the textbook conflict.
    world.directory.hold("jane", "Finance Approver");
    let conflicting = request("r-2", "jane", "Accounts Payable Clerk", admin);
    match gate.decide(conflicting).await {
        GrantOutcome::Blocked(codes) => assert!(!codes.is_empty()),
        other => panic!("a conflicting grant by a plain grantor is blocked, not {other:?}"),
    }
    // A grantor with the override right is told the conflict can be overridden, and nothing is recorded.
    let grantor = Grantor::new("auditor", ["override_sod"]);
    let asked = request("r-3", "jane", "Accounts Payable Clerk", grantor);
    assert!(matches!(
        gate.decide(asked).await,
        GrantOutcome::NeedsOverride(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_override_is_recorded_covers_the_same_grant_and_survives_a_restart() {
    let world = World::new();
    world.directory.hold("jane", "Finance Approver");
    let gate = world.gate(ApprovalPolicy::none(), false);
    let grantor = Grantor::new("auditor", ["override_sod"]);
    let overridden = with_override(request("o-1", "jane", "Accounts Payable Clerk", grantor));
    assert_eq!(gate.decide(overridden).await, GrantOutcome::Allowed);
    // The record covers exactly that triple: a later grant by a plain grantor passes the SoD check.
    let plain = request(
        "o-2",
        "jane",
        "Accounts Payable Clerk",
        Grantor::plain("admin"),
    );
    assert_eq!(gate.decide(plain.clone()).await, GrantOutcome::Allowed);
    // A restart: a new gate over the same store still honours it.
    let restarted = world.gate(ApprovalPolicy::none(), false);
    assert_eq!(restarted.decide(plain).await, GrantOutcome::Allowed);
    // Another user is not covered.
    world.directory.hold("kim", "Finance Approver");
    let other = request(
        "o-3",
        "kim",
        "Accounts Payable Clerk",
        Grantor::plain("admin"),
    );
    assert!(matches!(
        restarted.decide(other).await,
        GrantOutcome::Blocked(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_is_decided_by_the_approver_and_grants_exactly_once() {
    let world = World::new();
    let gate = world.gate(treasury_needs_approval(), true);
    let asked = request("a-1", "dave", "Treasury Clerk", Grantor::plain("admin"));
    let pending = gate.decide(asked.clone()).await;
    assert_eq!(
        pending,
        GrantOutcome::Pending {
            request: "a-1".to_owned(),
            case: "case-a-1".to_owned()
        }
    );
    // A retry of the request answers the same Case and launches nothing.
    assert_eq!(gate.decide(asked).await, pending);
    assert_eq!(world.launcher.launches.load(Ordering::SeqCst), 1);

    let completion = world.completion(gate);
    // The grantee and a stranger are refused; nothing is granted.
    for who in ["dave", "mallory"] {
        let refused = completion.complete(step_up(), "a-1", who, true, None).await;
        assert!(refused.is_err(), "{who} may not approve");
    }
    assert_eq!(world.executor.attempts.load(Ordering::SeqCst), 0);
    // The named approver completes it: granted once.
    let done = completion
        .complete(step_up(), "a-1", "carol", true, None)
        .await;
    assert_eq!(done, Ok(Completion::Granted));
    assert_eq!(*world.executor.granted.lock().unwrap(), ["grant:a-1"]);
    // A repeat executes nothing.
    let again = completion
        .complete(step_up(), "a-1", "carol", true, None)
        .await;
    assert_eq!(again, Ok(Completion::AlreadyGranted));
    assert_eq!(world.executor.attempts.load(Ordering::SeqCst), 1);
    // An unknown request is not pending.
    let unknown = completion
        .complete(step_up(), "nope", "carol", true, None)
        .await;
    assert_eq!(
        unknown,
        Err("iam_governance.approval_not_pending".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejection_grants_nothing_and_cannot_be_reversed() {
    let world = World::new();
    let gate = world.gate(treasury_needs_approval(), true);
    let asked = request("j-1", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert!(matches!(
        gate.decide(asked).await,
        GrantOutcome::Pending { .. }
    ));
    let completion = world.completion(gate);
    let rejected = completion
        .complete(step_up(), "j-1", "carol", false, Some("not now".to_owned()))
        .await;
    assert_eq!(rejected, Ok(Completion::Rejected));
    let late = completion
        .complete(step_up(), "j-1", "carol", true, None)
        .await;
    assert_eq!(late, Err("iam_governance.approval_not_pending".to_owned()));
    assert_eq!(world.executor.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_approval_survives_a_restart() {
    let world = World::new();
    let before = world.gate(treasury_needs_approval(), true);
    let asked = request("p-1", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert!(matches!(
        before.decide(asked.clone()).await,
        GrantOutcome::Pending { .. }
    ));
    drop(before);
    // The process restarts: a new gate, the same store. The approval is still pending and is completed.
    let after = world.gate(treasury_needs_approval(), true);
    let again = after.decide(asked).await;
    assert!(matches!(again, GrantOutcome::Pending { .. }));
    assert_eq!(
        world.launcher.launches.load(Ordering::SeqCst),
        1,
        "the restart launched nothing new"
    );
    let completion = world.completion(after);
    let done = completion
        .complete(step_up(), "p-1", "carol", true, None)
        .await;
    assert_eq!(done, Ok(Completion::Granted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_grant_after_approval_is_resumed_not_lost_and_not_doubled() {
    let world = World::new();
    let gate = world.gate(treasury_needs_approval(), true);
    let asked = request("f-1", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert!(matches!(
        gate.decide(asked).await,
        GrantOutcome::Pending { .. }
    ));
    let completion = world.completion(gate);
    world.executor.fail.store(true, Ordering::SeqCst);
    let failed = completion
        .complete(step_up(), "f-1", "carol", true, None)
        .await;
    assert!(failed.is_err());
    assert!(world.executor.granted.lock().unwrap().is_empty());
    // The executor recovers; the same approver completes again and the grant is made once.
    world.executor.fail.store(false, Ordering::SeqCst);
    let resumed = completion
        .complete(step_up(), "f-1", "carol", true, None)
        .await;
    assert_eq!(resumed, Ok(Completion::Granted));
    assert_eq!(world.executor.granted.lock().unwrap().len(), 1);
    // Someone else cannot take over the half-finished completion.
    let other = completion
        .complete(step_up(), "f-1", "erin", true, None)
        .await;
    assert!(other.is_err() || other == Ok(Completion::AlreadyGranted));
    assert_eq!(world.executor.granted.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_never_overrides_a_conflict_that_arose_meanwhile() {
    let world = World::new();
    let policy = ApprovalPolicy::none()
        .requiring_role("Accounts Payable Clerk")
        .routed_to("carol", "role:approver", 3_600);
    let gate = world.gate(policy, true);
    let asked = request(
        "c-1",
        "jane",
        "Accounts Payable Clerk",
        Grantor::plain("admin"),
    );
    assert!(matches!(
        gate.decide(asked).await,
        GrantOutcome::Pending { .. }
    ));
    // Before the approver answers, jane is given the role that conflicts.
    world.directory.hold("jane", "Finance Approver");
    let completion = world.completion(gate);
    let outcome = completion
        .complete(step_up(), "c-1", "carol", true, None)
        .await;
    assert!(matches!(outcome, Ok(Completion::Refused(codes)) if !codes.is_empty()));
    assert_eq!(world.executor.attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_failure_to_read_or_start_refuses_the_grant() {
    let world = World::new();
    // No launcher: a grant that needs approval is refused, never granted and never auto-approved.
    let no_launcher = world.gate(treasury_needs_approval(), false);
    let asked = request("u-1", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert_eq!(
        no_launcher.decide(asked.clone()).await,
        GrantOutcome::Unavailable("governance_approval_host.unavailable")
    );
    // The directory cannot be read: everything is refused, even a clean grant.
    let gate = world.gate(ApprovalPolicy::none(), false);
    world.directory.broken.store(true, Ordering::SeqCst);
    assert_eq!(
        gate.decide(asked).await,
        GrantOutcome::Unavailable("governance_host.state_unavailable")
    );
    // A SoD file that did not parse refuses everything too.
    let world = World::new();
    let slot = Arc::new(LauncherSlot::default());
    let store: Arc<dyn GovernanceStore> = world.store.clone();
    let broken = Governance::from_parts(Parts {
        site: "acme".to_owned(),
        source: world.directory.clone(),
        store,
        clock: Arc::clone(&world.clock),
        rules: None,
        catalogue: None,
        delegation: DelegationPolicy::Unrestricted,
        approval: ApprovalPolicy::none(),
        launcher: slot,
        broken: true,
    });
    let clean = request("u-2", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert_eq!(
        broken.decide(clean).await,
        GrantOutcome::Unavailable("governance_host.state_unavailable")
    );
    // Without an executor an approved grant cannot be made and stays recorded.
    let world = World::new();
    let gate = world.gate(treasury_needs_approval(), true);
    let asked = request("u-3", "dave", "Treasury Clerk", Grantor::plain("admin"));
    assert!(matches!(
        gate.decide(asked).await,
        GrantOutcome::Pending { .. }
    ));
    let completion = GrantCompletion::new(Arc::new(gate), None);
    let result = completion
        .complete(step_up(), "u-3", "carol", true, None)
        .await;
    assert_eq!(
        result,
        Err("governance_approval_host.unavailable".to_owned())
    );
}
