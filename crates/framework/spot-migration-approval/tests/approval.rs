//! Behaviour of plan approval: a modified or unapproved plan is refused at apply.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, deprecated)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use sha2::{Digest, Sha256};
use spot_audit_chain::{AuditChain, CheckpointPolicy, InMemoryStore, LocalSigner};
use spot_migration_approval::cli::{self, Command, CommandSet};
use spot_migration_approval::{
    Act, Approval, ApprovalPolicy, ApprovalService, ApprovalStore, AuthoriseError, AuthorshipCheck,
    Checks, Decision, Identity, InMemoryApprovalStore, MemoryAuditSink, PlanDigest, Refusal,
    SignatureHook, authorise_apply, codes,
};
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, ReleasePlan, Risk, SchemaPlan, SystemDescription, plan,
};

const HOUR: i64 = 3_600_000_000;
const T0: i64 = 1_000 * HOUR;

fn plan_with_risk(risk: Risk) -> ReleasePlan {
    let empty = SystemDescription::new();
    let mut release = plan(&empty, &empty, &SchemaPlan::default());
    release.changes.push(Change::new(
        "schema/example",
        ChangeKind::SchemaAddColumn,
        "schema/add_column",
        "invoice.note",
        "Add a column",
        risk,
        BlockingClass::Online,
    ));
    release.risk = risk;
    release
}

fn approval(plan: &ReleasePlan, who: &str, role: &str, decision: Decision, at: i64) -> Approval {
    Approval {
        plan_digest: PlanDigest::of(plan),
        approver: who.to_owned(),
        role: role.to_owned(),
        decision,
        comment: String::new(),
        at,
        signature: None,
    }
}

fn approve(plan: &ReleasePlan, who: &str, at: i64) -> Approval {
    approval(plan, who, "release_manager", Decision::Approve, at)
}

fn go(
    plan: &ReleasePlan,
    approvals: &[Approval],
    policy: &ApprovalPolicy,
    now: i64,
) -> Result<spot_migration_approval::ApplyTicket, Refusal> {
    authorise_apply(plan, approvals, policy, now, &Checks::none())
}

struct Author(&'static str);
impl AuthorshipCheck for Author {
    fn is_author(&self, _plan: &PlanDigest, who: &str) -> bool {
        who == self.0
    }
}

struct Hook;
impl SignatureHook for Hook {
    fn verify(&self, _digest: &PlanDigest, approval: &Approval) -> bool {
        approval.signature.as_deref() == Some("good")
    }
}

#[test]
fn digest_is_domain_separated_sha256_of_the_plan_text() {
    let release = plan_with_risk(Risk::NeedsReview);
    let mut hasher = Sha256::new();
    hasher.update(b"spotledger-migration-plan-v1\0");
    hasher.update(release.json().as_bytes());
    let expected: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(PlanDigest::of(&release).as_hex(), expected);
}

#[test]
fn unapproved_plan_is_refused() {
    let release = plan_with_risk(Risk::NeedsReview);
    let refusal = go(&release, &[], &ApprovalPolicy::default(), T0).unwrap_err();
    assert_eq!(
        refusal,
        Refusal::NotEnoughApprovals {
            valid: 0,
            required: 1
        }
    );
    assert_eq!(refusal.code(), codes::NOT_ENOUGH_APPROVALS);
}

#[test]
fn modified_step_after_approval_is_refused() {
    let release = plan_with_risk(Risk::NeedsReview);
    let approvals = [approve(&release, "ann", T0)];
    let policy = ApprovalPolicy::default();
    let ticket = go(&release, &approvals, &policy, T0 + HOUR).unwrap();
    assert!(ticket.matches(&release));

    let mut changed = release.clone();
    changed.changes[0].description = "Add a different column".to_owned();
    assert_eq!(
        go(&changed, &approvals, &policy, T0 + HOUR),
        Err(Refusal::PlanChanged)
    );
    assert!(!ticket.matches(&changed));

    let mut extra = release.clone();
    extra.changes.push(release.changes[0].clone());
    assert_eq!(
        go(&extra, &approvals, &policy, T0 + HOUR),
        Err(Refusal::PlanChanged)
    );
}

#[test]
fn a_forged_lower_risk_field_does_not_lower_the_requirement() {
    let mut release = plan_with_risk(Risk::Destructive);
    release.risk = Risk::Additive;
    let refusal = go(&release, &[], &ApprovalPolicy::default(), T0).unwrap_err();
    assert_eq!(
        refusal,
        Refusal::NotEnoughApprovals {
            valid: 0,
            required: 2
        }
    );
}

#[test]
fn expired_approval_is_refused() {
    let release = plan_with_risk(Risk::NeedsReview);
    let policy = ApprovalPolicy::default();
    let approvals = [approve(&release, "ann", T0)];
    assert!(go(&release, &approvals, &policy, T0 + policy.expiry_micros - 1).is_ok());
    assert_eq!(
        go(&release, &approvals, &policy, T0 + policy.expiry_micros),
        Err(Refusal::Expired)
    );
}

#[test]
fn approval_dated_in_the_future_does_not_count() {
    let release = plan_with_risk(Risk::NeedsReview);
    let approvals = [approve(&release, "ann", T0 + HOUR)];
    assert!(go(&release, &approvals, &ApprovalPolicy::default(), T0).is_err());
}

#[test]
fn policy_matrix_by_risk_class() {
    let policy = ApprovalPolicy::default();

    // Additive: auto-approved, no approver needed, and the ticket says so.
    let additive = plan_with_risk(Risk::Additive);
    let ticket = go(&additive, &[], &policy, T0).unwrap();
    assert!(ticket.approvers().is_empty());
    assert_eq!(ticket.risk(), Risk::Additive);

    // Additive with auto-approve off needs one approver.
    let mut strict = policy.clone();
    strict.additive.auto_approve = false;
    strict.additive.role = None;
    assert!(go(&additive, &[], &strict, T0).is_err());
    assert!(go(&additive, &[approve(&additive, "ann", T0)], &strict, T0).is_ok());

    // Needs review: one approver in the role; a wrong role does not count.
    let review = plan_with_risk(Risk::NeedsReview);
    let wrong_role = approval(&review, "bob", "clerk", Decision::Approve, T0);
    assert!(go(&review, &[wrong_role], &policy, T0).is_err());
    let mut two = policy.clone();
    two.needs_review.approvers = 2;
    let ann = approve(&review, "ann", T0);
    assert_eq!(
        go(&review, std::slice::from_ref(&ann), &two, T0),
        Err(Refusal::NotEnoughApprovals {
            valid: 1,
            required: 2
        })
    );
    assert!(go(&review, &[ann, approve(&review, "bob", T0)], &two, T0).is_ok());

    // Destructive: two people.
    let destructive = plan_with_risk(Risk::Destructive);
    let ann = approve(&destructive, "ann", T0);
    assert!(go(&destructive, std::slice::from_ref(&ann), &policy, T0).is_err());
    let ticket = go(
        &destructive,
        &[ann, approve(&destructive, "bob", T0)],
        &policy,
        T0,
    )
    .unwrap();
    assert_eq!(ticket.approvers(), ["ann".to_owned(), "bob".to_owned()]);
}

#[test]
fn same_approver_twice_counts_once() {
    let destructive = plan_with_risk(Risk::Destructive);
    let approvals = [
        approve(&destructive, "ann", T0),
        approve(&destructive, "ann", T0 + 1),
    ];
    assert_eq!(
        go(&destructive, &approvals, &ApprovalPolicy::default(), T0 + 2),
        Err(Refusal::NotEnoughApprovals {
            valid: 1,
            required: 2
        })
    );
}

#[test]
fn reject_blocks_until_the_same_approver_approves_again() {
    let release = plan_with_risk(Risk::NeedsReview);
    let policy = ApprovalPolicy::default();
    let mut log = vec![approve(&release, "ann", T0)];
    log.push(approval(
        &release,
        "ann",
        "release_manager",
        Decision::Reject,
        T0 + 1,
    ));
    assert_eq!(
        go(&release, &log, &policy, T0 + 2),
        Err(Refusal::Rejected {
            by: "ann".to_owned()
        })
    );
    log.push(approve(&release, "ann", T0 + 3));
    assert!(go(&release, &log, &policy, T0 + 4).is_ok());

    // Another approver's rejection still blocks while ann's approval stands.
    log.push(approval(
        &release,
        "bob",
        "release_manager",
        Decision::Reject,
        T0 + 5,
    ));
    assert!(matches!(
        go(&release, &log, &policy, T0 + 6),
        Err(Refusal::Rejected { .. })
    ));
}

#[test]
fn author_cannot_approve_when_the_policy_says_so() {
    let release = plan_with_risk(Risk::NeedsReview);
    let author = Author("ann");
    let checks = Checks {
        authorship: &author,
        signature: None,
    };
    let policy = ApprovalPolicy::default();
    // Injected records from the author are ignored at apply time.
    let approvals = [approve(&release, "ann", T0)];
    assert!(authorise_apply(&release, &approvals, &policy, T0, &checks).is_err());
    // And the service refuses to record it.
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        policy.clone(),
    )
    .unwrap();
    service
        .register_plan(&PlanDigest::of(&release), "ann")
        .unwrap();
    let act = Act {
        identity: Identity::new("ann", &["release_manager"]),
        role: "release_manager",
        comment: "mine",
        signature: None,
        now: T0,
    };
    let error = service.approve(&release, &act, &checks).unwrap_err();
    assert_eq!(error.code(), codes::NOT_PERMITTED);
    assert!(service.audit().records().is_empty());
    // With the rule off, the author may approve.
    let mut lax = policy;
    lax.forbid_author_approval = false;
    assert!(authorise_apply(&release, &approvals, &lax, T0, &checks).is_ok());
}

#[test]
fn hardware_signature_is_required_when_the_policy_asks() {
    let destructive = plan_with_risk(Risk::Destructive);
    let mut policy = ApprovalPolicy::default();
    policy.destructive.require_hardware_signature = true;
    let mut ann = approve(&destructive, "ann", T0);
    let mut bob = approve(&destructive, "bob", T0);
    ann.signature = Some("good".to_owned());

    // No hook wired: refused even with a signature present.
    assert!(matches!(
        go(&destructive, &[ann.clone(), bob.clone()], &policy, T0),
        Err(Refusal::SignatureRequired { .. })
    ));
    let checks = Checks {
        authorship: &spot_migration_approval::NoAuthors,
        signature: Some(&Hook),
    };
    // Bob has no signature.
    assert_eq!(
        authorise_apply(
            &destructive,
            &[ann.clone(), bob.clone()],
            &policy,
            T0,
            &checks
        ),
        Err(Refusal::SignatureRequired {
            approver: "bob".to_owned()
        })
    );
    bob.signature = Some("bad".to_owned());
    assert!(
        authorise_apply(
            &destructive,
            &[ann.clone(), bob.clone()],
            &policy,
            T0,
            &checks
        )
        .is_err()
    );
    bob.signature = Some("good".to_owned());
    assert!(authorise_apply(&destructive, &[ann, bob], &policy, T0, &checks).is_ok());
}

#[test]
fn policy_validation_and_settings_round_trip() {
    let policy = ApprovalPolicy::default();
    assert_eq!(
        ApprovalPolicy::from_setting(&policy.to_setting()).unwrap(),
        policy
    );
    let mut weak = policy.clone();
    weak.destructive.approvers = 1;
    assert_eq!(weak.validate().unwrap_err().code(), codes::BAD_POLICY);
    assert!(ApprovalPolicy::from_setting(&weak.to_setting()).is_err());
    assert!(ApprovalPolicy::from_setting("not json").is_err());
    assert_eq!(
        go(&plan_with_risk(Risk::Additive), &[], &weak, T0),
        Err(Refusal::BadPolicy {
            message:
                "migration_approval.bad_policy: a destructive plan needs at least two approvers"
                    .to_owned()
        })
    );
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

#[test]
fn every_action_is_audited_and_lands_on_the_audit_chain() {
    let destructive = plan_with_risk(Risk::Destructive);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    let checks = Checks::none();
    service
        .register_plan(&PlanDigest::of(&destructive), "zed")
        .unwrap();
    let act = |who: &str, now| Act {
        identity: Identity::new(who, &["release_manager"]),
        role: "release_manager",
        comment: "looks right",
        signature: None,
        now,
    };

    service.record_shown(&destructive, "ann", T0).unwrap();
    service
        .reject(&destructive, &act("ann", T0 + 1), &checks)
        .unwrap();
    assert_eq!(
        service
            .authorise_apply(&destructive, "ann", T0 + 2, &checks)
            .unwrap_err(),
        AuthoriseError::Refused(Refusal::Rejected {
            by: "ann".to_owned()
        })
    );
    service
        .approve(&destructive, &act("ann", T0 + 3), &checks)
        .unwrap();
    service
        .approve(&destructive, &act("bob", T0 + 4), &checks)
        .unwrap();
    let status = service.status(&destructive, T0 + 5, &checks).unwrap();
    assert!(status.ready);
    assert_eq!(status.approved_by, ["ann", "bob"]);
    let ticket = service
        .authorise_apply(&destructive, "ann", T0 + 6, &checks)
        .unwrap();
    assert!(ticket.matches(&destructive));

    let records = service.audit().records();
    let kinds: Vec<&str> = records.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "migration.plan_shown",
            "migration.rejected",
            "migration.apply_refused",
            "migration.approved",
            "migration.approved",
            "migration.apply_authorised",
        ]
    );
    assert!(records.iter().all(|r| r.actor.starts_with("user:")));

    // The records are valid for the real chain and read back in order.
    let chain = AuditChain::new(
        InMemoryStore::new(),
        "site-1",
        Arc::new(LocalSigner::from_seed([7; 32])),
        CheckpointPolicy::default(),
    );
    for record in records {
        block_on(chain.append(record)).unwrap();
    }
    let entries = block_on(chain.read_range(0, 100)).unwrap();
    assert_eq!(entries.len(), 6);
}

#[test]
fn a_failing_audit_sink_stops_the_action() {
    struct Broken;
    impl spot_migration_approval::AuditSink for Broken {
        fn record(
            &self,
            _record: spot_audit_chain::AuditRecord,
        ) -> Result<(), spot_migration_approval::ApprovalError> {
            Err(spot_migration_approval::ApprovalError::Backend {
                message: "down".to_owned(),
            })
        }
    }
    let release = plan_with_risk(Risk::NeedsReview);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        Broken,
        ApprovalPolicy::default(),
    )
    .unwrap();
    service
        .register_plan(&PlanDigest::of(&release), "zed")
        .unwrap();
    let act = Act {
        identity: Identity::new("ann", &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    assert_eq!(
        service
            .approve(&release, &act, &Checks::none())
            .unwrap_err()
            .code(),
        codes::AUDIT_UNAVAILABLE
    );
    assert_eq!(service.store().all().unwrap().len(), 0);
}

#[derive(Default)]
struct Registry(Vec<String>);
impl CommandSet for Registry {
    fn register(&mut self, path: &str, _about: &str) {
        self.0.push(path.to_owned());
    }
}

#[test]
fn cli_subcommands_return_structured_output() {
    let mut registry = Registry::default();
    cli::register(&mut registry);
    assert_eq!(registry.0, ["plan show", "approve", "reject", "status"]);

    let release = plan_with_risk(Risk::NeedsReview);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    service
        .register_plan(&PlanDigest::of(&release), "zed")
        .unwrap();
    let checks = Checks::none();
    let run = |args: &[&str], who: &str, now: i64| {
        cli::run(
            &service,
            &release,
            &Command::parse(args).unwrap(),
            &Identity::new(who, &["clerk", "release_manager"]),
            now,
            &checks,
        )
    };

    let shown = run(&["plan", "show"], "ann", T0);
    assert!(shown.ok);
    assert_eq!(shown.data["status"]["ready"], false);
    assert_eq!(
        shown.data["status"]["blocked_by"],
        codes::NOT_ENOUGH_APPROVALS
    );

    let denied = run(&["approve", "--role", "clerk"], "bob", T0);
    assert!(!denied.ok);
    assert_eq!(denied.code.as_deref(), Some(codes::NOT_PERMITTED));

    let approved = run(
        &["approve", "--role", "release_manager", "--comment", "ok"],
        "bob",
        T0 + 1,
    );
    assert!(approved.ok);
    assert_eq!(approved.data["status"]["ready"], true);
    assert_eq!(
        run(&["status"], "bob", T0 + 2).data["status"]["approved_by"][0],
        "bob"
    );

    let rejected = run(&["reject", "--role", "release_manager"], "bob", T0 + 3);
    assert_eq!(rejected.data["status"]["blocked_by"], codes::REJECTED);

    let gate = cli::authorise(&service, &release, "bob", T0 + 4, &checks);
    assert_eq!(gate.code.as_deref(), Some(codes::REJECTED));

    for bad in [
        &["frobnicate"][..],
        &["approve"],
        &["approve", "--role"],
        &["status", "--role", "x"],
    ] {
        assert_eq!(Command::parse(bad).unwrap_err().code(), codes::BAD_COMMAND);
    }
}

#[test]
fn a_role_the_identity_does_not_hold_is_refused() {
    let release = plan_with_risk(Risk::NeedsReview);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    service
        .register_plan(&PlanDigest::of(&release), "zed")
        .unwrap();
    let claim = |decision_is_approve: bool| {
        let act = Act {
            identity: Identity::new("bob", &["clerk"]),
            role: "release_manager",
            comment: "",
            signature: None,
            now: T0,
        };
        if decision_is_approve {
            service.approve(&release, &act, &Checks::none())
        } else {
            service.reject(&release, &act, &Checks::none())
        }
    };
    for approve in [true, false] {
        assert_eq!(claim(approve).unwrap_err().code(), codes::ROLE_NOT_HELD);
    }
    assert!(service.store().all().unwrap().is_empty());
    assert!(service.audit().records().is_empty());
    // And through the command line: the role is only a choice among the identity's roles.
    let output = cli::run(
        &service,
        &release,
        &Command::parse(&["approve", "--role", "release_manager"]).unwrap(),
        &Identity::new("bob", &["clerk"]),
        T0,
        &Checks::none(),
    );
    assert_eq!(output.code.as_deref(), Some(codes::ROLE_NOT_HELD));
    // An identity with no subject cannot act.
    let nobody = Act {
        identity: Identity::new("", &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    assert_eq!(
        service
            .approve(&release, &nobody, &Checks::none())
            .unwrap_err()
            .code(),
        codes::NOT_PERMITTED
    );
}

#[test]
fn an_unknown_author_fails_closed_and_a_conflicting_author_is_refused() {
    let release = plan_with_risk(Risk::NeedsReview);
    let digest = PlanDigest::of(&release);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    let act = |who: &str| Act {
        identity: Identity::new(who, &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    // No registered author: refused, whoever asks, and nothing is audited or stored.
    let error = service
        .approve(&release, &act("bob"), &Checks::none())
        .unwrap_err();
    assert_eq!(error.code(), codes::AUTHOR_UNKNOWN);
    assert!(service.store().all().unwrap().is_empty());
    assert!(service.audit().records().is_empty());

    // The author is set once; the same author again is a no-op, another author is a conflict.
    service.register_plan(&digest, "ann").unwrap();
    service.register_plan(&digest, "ann").unwrap();
    assert_eq!(
        service.register_plan(&digest, "bob").unwrap_err().code(),
        codes::AUTHOR_CONFLICT
    );
    assert_eq!(
        service.store().author_of(&digest).unwrap().as_deref(),
        Some("ann")
    );
    assert_eq!(
        service.register_plan(&digest, " ").unwrap_err().code(),
        codes::NOT_PERMITTED
    );
    // Another plan is still unknown.
    let other = plan_with_risk(Risk::Destructive);
    assert_eq!(
        service
            .approve(&other, &act("bob"), &Checks::none())
            .unwrap_err()
            .code(),
        codes::AUTHOR_UNKNOWN
    );
    service
        .approve(&release, &act("bob"), &Checks::none())
        .unwrap();
}

#[test]
fn the_author_stays_unable_to_approve_after_a_restart() {
    let release = plan_with_risk(Risk::NeedsReview);
    let digest = PlanDigest::of(&release);
    let store = Arc::new(InMemoryApprovalStore::new());
    let act = |who: &str| Act {
        identity: Identity::new(who, &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    let first = ApprovalService::new(
        store.clone(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    first.register_plan(&digest, "ann").unwrap();
    drop(first);

    // A new service over the same store: no process memory, the same answer.
    let second = ApprovalService::new(
        store.clone(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    assert_eq!(
        second
            .approve(&release, &act("ann"), &Checks::none())
            .unwrap_err()
            .code(),
        codes::NOT_PERMITTED
    );
    second
        .approve(&release, &act("bob"), &Checks::none())
        .unwrap();

    // A restart that lost the plan record cannot approve at all.
    let amnesiac = ApprovalService::new(
        Arc::new(InMemoryApprovalStore::new()),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    assert_eq!(
        amnesiac
            .approve(&release, &act("ann"), &Checks::none())
            .unwrap_err()
            .code(),
        codes::AUTHOR_UNKNOWN
    );
}

#[test]
fn a_store_that_cannot_persist_authors_refuses_every_approval() {
    struct NoAuthorStore(InMemoryApprovalStore);
    impl ApprovalStore for NoAuthorStore {
        fn append(&self, a: Approval) -> Result<(), spot_migration_approval::ApprovalError> {
            self.0.append(a)
        }
        fn for_digest(
            &self,
            d: &PlanDigest,
        ) -> Result<Vec<Approval>, spot_migration_approval::ApprovalError> {
            self.0.for_digest(d)
        }
        fn all(&self) -> Result<Vec<Approval>, spot_migration_approval::ApprovalError> {
            self.0.all()
        }
    }
    let release = plan_with_risk(Risk::NeedsReview);
    let service = ApprovalService::new(
        NoAuthorStore(InMemoryApprovalStore::new()),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap();
    assert_eq!(
        service
            .register_plan(&PlanDigest::of(&release), "ann")
            .unwrap_err()
            .code(),
        codes::BACKEND
    );
    let act = Act {
        identity: Identity::new("bob", &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    assert_eq!(
        service
            .approve(&release, &act, &Checks::none())
            .unwrap_err()
            .code(),
        codes::AUTHOR_UNKNOWN
    );
}
