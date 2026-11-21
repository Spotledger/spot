//! Registering a plan's author is audited as `migration.plan_registered` (WP-MIG-04.T4).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_migration_approval::service::kinds;
use spot_migration_approval::{
    Act, ApprovalError, ApprovalPolicy, ApprovalService, ApprovalStore, AuditSink, Checks,
    Identity, InMemoryApprovalStore, MemoryAuditSink, PlanDigest, codes,
};
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, ReleasePlan, Risk, SchemaPlan, SystemDescription, plan,
};

const T0: i64 = 3_600_000_000_000;

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

fn service() -> ApprovalService<InMemoryApprovalStore, MemoryAuditSink> {
    ApprovalService::new(
        InMemoryApprovalStore::new(),
        MemoryAuditSink::new(),
        ApprovalPolicy::default(),
    )
    .unwrap()
}

struct Broken;
impl AuditSink for Broken {
    fn record(&self, _record: AuditRecord) -> Result<(), ApprovalError> {
        Err(ApprovalError::Backend {
            message: "down".to_owned(),
        })
    }
}

#[test]
fn the_first_registration_writes_exactly_one_record() {
    let release = plan_with_risk(Risk::NeedsReview);
    let digest = PlanDigest::of(&release);
    let service = service();
    service.register_plan_at(&release, "ann", T0).unwrap();
    let records = service.audit().records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.kind, kinds::PLAN_REGISTERED);
    assert_eq!(record.actor, "user:ann");
    let subject = record.subject.as_ref().unwrap();
    assert_eq!(subject.doctype, "MigrationPlan");
    assert_eq!(subject.id, digest.to_string());
    assert_eq!(record.at, T0);
    assert!(record.masked);
    assert_eq!(
        record.payload.get("action"),
        Some(&PayloadValue::Text("register".to_owned()))
    );
    assert_eq!(
        record.payload.get("author"),
        Some(&PayloadValue::Text("ann".to_owned()))
    );
    assert_eq!(
        record.payload.get("risk"),
        Some(&PayloadValue::Text("needs_review".to_owned()))
    );
    assert_eq!(
        service.store().author_of(&digest).unwrap().as_deref(),
        Some("ann")
    );
}

#[test]
fn the_same_call_again_writes_no_second_record() {
    let release = plan_with_risk(Risk::NeedsReview);
    let service = service();
    service.register_plan_at(&release, "ann", T0).unwrap();
    service.register_plan_at(&release, "ann", T0 + 1).unwrap();
    assert_eq!(service.audit().records().len(), 1);
}

#[test]
fn a_different_author_is_refused_and_writes_no_record() {
    let release = plan_with_risk(Risk::NeedsReview);
    let digest = PlanDigest::of(&release);
    let service = service();
    service.register_plan_at(&release, "ann", T0).unwrap();
    let error = service
        .register_plan_at(&release, "bob", T0 + 1)
        .unwrap_err();
    assert_eq!(error.code(), codes::AUTHOR_CONFLICT);
    assert_eq!(service.audit().records().len(), 1);
    assert_eq!(
        service.store().author_of(&digest).unwrap().as_deref(),
        Some("ann")
    );
}

#[test]
fn an_empty_author_is_refused_and_writes_no_record() {
    let release = plan_with_risk(Risk::NeedsReview);
    let service = service();
    let error = service.register_plan_at(&release, "  ", T0).unwrap_err();
    assert_eq!(error.code(), codes::NOT_PERMITTED);
    assert!(service.audit().records().is_empty());
}

#[test]
fn an_audit_failure_leaves_the_plan_unregistered() {
    let release = plan_with_risk(Risk::NeedsReview);
    let digest = PlanDigest::of(&release);
    let service = ApprovalService::new(
        InMemoryApprovalStore::new(),
        Broken,
        ApprovalPolicy::default(),
    )
    .unwrap();
    let error = service.register_plan_at(&release, "ann", T0).unwrap_err();
    assert_eq!(error.code(), codes::AUDIT_UNAVAILABLE);
    assert_eq!(service.store().author_of(&digest).unwrap(), None);
    // A later approval is refused: the author is unknown.
    let act = Act {
        identity: Identity::new("bob", &["release_manager"]),
        role: "release_manager",
        comment: "",
        signature: None,
        now: T0,
    };
    let error = service
        .approve(&release, &act, &Checks::none())
        .unwrap_err();
    assert_eq!(error.code(), codes::AUTHOR_UNKNOWN);
}

#[test]
fn the_record_carries_no_signature_or_comment_text() {
    let release = plan_with_risk(Risk::NeedsReview);
    let service = service();
    service.register_plan_at(&release, "ann", T0).unwrap();
    let record = &service.audit().records()[0];
    let keys: Vec<&str> = record.payload.keys().map(String::as_str).collect();
    assert_eq!(keys, ["action", "author", "risk"]);
    assert!(!keys.contains(&"comment"));
    assert!(!keys.contains(&"signature"));
    assert!(!keys.contains(&"signed"));
}
