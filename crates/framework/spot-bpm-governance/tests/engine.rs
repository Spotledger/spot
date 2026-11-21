//! The guard at the document engine's pre-create hook: every channel that creates a record runs
//! it, because they all run the record's own create lifecycle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, Value};
use spot_app_api::{AppError, Controller, Document};
use spot_bpm_governance::{
    AuditKind, GovernanceController, GovernanceMode, MemoryPolicyStore, MemoryRequests,
    RecordingAudit, RequestAuthority, actor_text,
};
use spot_core::SiteId;
use spot_document::engine::Engine;
use spot_storage_fake::FakeBackend;

/// The engine, the guard's ports and the controller for `note`.
struct Fixture {
    engine: Arc<Engine<FakeBackend>>,
    policy: Arc<MemoryPolicyStore>,
    audit: Arc<RecordingAudit>,
}

fn build(principal: spot_permissions::Principal, requests: Arc<MemoryRequests>) -> Fixture {
    let ports = Ports {
        policy: MemoryPolicyStore::new(8).shared(),
        requests,
        audit: RecordingAudit::new().shared(),
    };
    let guard = guard(&ports);
    let controller: Arc<dyn Controller> = Arc::new(GovernanceController::new(
        "note",
        guard,
        principals(principal),
    ));
    let engine = engine_with(vec![("bpm".to_owned(), controller)]);
    Fixture {
        engine,
        policy: ports.policy,
        audit: ports.audit,
    }
}

fn fixture(principal: spot_permissions::Principal) -> Fixture {
    build(principal, MemoryRequests::new().shared())
}

/// Issues the grant a running step would, as the authority binds it to the acting person.
fn grant(
    requests: &MemoryRequests,
    actor: &Actor,
    request: &str,
    node: &str,
    attempt: u32,
) -> String {
    requests
        .issue(request, node, attempt, &actor_text(actor))
        .unwrap()
}

fn invocation(actor: Actor, command_id: Option<&str>) -> Invocation {
    Invocation {
        site: SiteId::new("acme.test").unwrap(),
        actor,
        command_id: command_id.and_then(|c| CommandId::new(c).ok()),
        language: None,
        now: now(),
    }
}

fn note(company: &str, reason: Option<&str>) -> Document {
    let mut doc = Document::new(n("note"), RecordId::new_v7());
    doc.set(n("company"), Value::Text(company.to_owned()));
    doc.set(n("title"), Value::Text("A note".to_owned()));
    if let Some(reason) = reason {
        doc.set(n("outside_process_reason"), Value::Text(reason.to_owned()));
    }
    doc
}

fn code(error: &AppError) -> &str {
    &error.code
}

#[test]
fn a_free_create_is_allowed() {
    let f = fixture(plain_principal());
    let outcome = block_on(
        f.engine
            .create(&invocation(user_actor(), None), note("acme", None)),
    );
    assert!(outcome.is_ok(), "governance is off by default");
    assert!(f.audit.is_empty());
}

#[test]
fn a_governed_create_is_refused_through_every_channel() {
    let f = fixture(plain_principal());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);

    // The native API (a user), an import (a system component) and a server script (the
    // triggering user) all create through the same lifecycle, so all three are refused.
    for actor in [user_actor(), import_actor(), script_actor()] {
        let error = block_on(
            f.engine
                .create(&invocation(actor, None), note("acme", None)),
        )
        .expect_err("a governed create outside its request is refused");
        assert_eq!(code(&error), "bpm_governance.outside_process_denied");
    }
    assert_eq!(f.audit.len(), 3, "every refusal is audited");
    assert!(
        f.audit
            .entries()
            .iter()
            .all(|entry| entry.kind == AuditKind::Refused)
    );
}

#[test]
fn a_governed_create_inside_a_case_is_allowed() {
    let requests = MemoryRequests::new().shared();
    requests.allow("case-1", "node-1", "note");
    let f = build(plain_principal(), requests.clone());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let key = grant(&requests, &user_actor(), "case-1", "node-1", 1);
    let outcome = block_on(
        f.engine
            .create(&invocation(user_actor(), Some(&key)), note("acme", None)),
    );
    assert!(outcome.is_ok(), "a verified grant permits the create");
    assert!(f.audit.is_empty());
}

/// A command id someone typed is not a request context, however well shaped.
#[test]
fn a_forged_request_key_does_not_permit_a_governed_create() {
    let requests = MemoryRequests::new().shared();
    requests.allow("case-1", "node-1", "note");
    let f = build(plain_principal(), requests.clone());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    // A real request runs at the node that may create `note`, so the shapes below name a
    // genuine request; only the grant is missing.
    let real = grant(&requests, &user_actor(), "case-1", "node-1", 1);
    assert!(
        block_on(
            f.engine
                .create(&invocation(user_actor(), Some(&real)), note("acme", None))
        )
        .is_ok(),
        "the issued grant is the control"
    );
    let before = f.audit.len();

    for forged in [
        "case-1:node-1:1", // the old wire form: a copied request id proves nothing
        "case-1:node-1:0",
        "case-1:node-1:99",
        "grant-1",
    ] {
        for actor in [user_actor(), import_actor(), script_actor()] {
            let error = block_on(
                f.engine
                    .create(&invocation(actor, Some(forged)), note("acme", None)),
            )
            .expect_err("a typed command id is not a request context");
            assert_eq!(
                code(&error),
                "bpm_governance.outside_process_denied",
                "{forged}"
            );
        }
    }
    assert_eq!(f.audit.len(), before + 12, "every refusal is audited");
    assert!(
        f.audit.entries()[before..]
            .iter()
            .all(|entry| entry.kind == AuditKind::Refused)
    );
}

#[test]
fn a_grant_issued_to_another_person_is_refused() {
    let requests = MemoryRequests::new().shared();
    requests.allow("case-1", "node-1", "note");
    let f = build(plain_principal(), requests.clone());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    // The step runs as the import component; a user who copied its key is not it.
    let key = grant(&requests, &import_actor(), "case-1", "node-1", 1);
    let error = block_on(
        f.engine
            .create(&invocation(user_actor(), Some(&key)), note("acme", None)),
    )
    .expect_err("the grant is bound to the acting person");
    assert_eq!(code(&error), "bpm_governance.outside_process_denied");
    assert!(
        block_on(
            f.engine
                .create(&invocation(import_actor(), Some(&key)), note("acme", None))
        )
        .is_ok(),
        "the person it was issued to is accepted"
    );
}

#[test]
fn a_replayed_old_attempt_is_refused() {
    let requests = MemoryRequests::new().shared();
    requests.allow("case-1", "node-1", "note");
    let f = build(plain_principal(), requests.clone());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let first = grant(&requests, &user_actor(), "case-1", "node-1", 1);
    let second = grant(&requests, &user_actor(), "case-1", "node-1", 2);
    let replay = block_on(
        f.engine
            .create(&invocation(user_actor(), Some(&first)), note("acme", None)),
    )
    .expect_err("attempt 1 is over once attempt 2 is issued");
    assert_eq!(code(&replay), "bpm_governance.outside_process_denied");
    assert!(
        block_on(
            f.engine
                .create(&invocation(user_actor(), Some(&second)), note("acme", None))
        )
        .is_ok()
    );
}

/// A verified step at a node that may not create the type is refused, not a permission route.
#[test]
fn a_verified_step_that_may_not_create_is_refused() {
    let requests = MemoryRequests::new().shared();
    requests.allow("case-1", "node-2", "note");
    let f = build(plain_principal(), requests.clone());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let key = grant(&requests, &user_actor(), "case-1", "node-1", 1);
    let error = block_on(
        f.engine
            .create(&invocation(user_actor(), Some(&key)), note("acme", None)),
    )
    .expect_err("the verified node may not create this type");
    assert_eq!(code(&error), "bpm_governance.outside_process_denied");
}

#[test]
fn a_governed_create_with_the_outside_process_permission_is_allowed_and_audited() {
    let f = fixture(outside_principal());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let outcome = block_on(f.engine.create(
        &invocation(user_actor(), None),
        note("acme", Some("approved by the controller")),
    ));
    assert!(outcome.is_ok(), "the audited exception is allowed");
    let entries = f.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, AuditKind::OutsideProcess);
}

#[test]
fn a_governed_create_with_a_blank_reason_is_refused() {
    let f = fixture(outside_principal());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let error = block_on(
        f.engine
            .create(&invocation(user_actor(), None), note("acme", Some("   "))),
    )
    .expect_err("a blank reason is refused");
    assert_eq!(code(&error), "bpm_governance.reason_required");
}

#[test]
fn process_only_removes_the_outside_process_route() {
    let f = fixture(outside_principal());
    f.policy
        .set_mode("acme", "note", GovernanceMode::ProcessOnly);
    // Governance defers to the entry crate: no permission route is offered here, and nothing is
    // audited by this crate.
    let outcome = block_on(f.engine.create(
        &invocation(user_actor(), None),
        note("acme", Some("approved by the controller")),
    ));
    assert!(
        outcome.is_ok(),
        "this crate defers process_only to the entry crate"
    );
    assert!(
        f.audit.is_empty(),
        "process_only is not enforced or audited here"
    );
}

#[test]
fn another_company_is_unaffected_by_a_governed_one() {
    let f = fixture(plain_principal());
    f.policy.set_mode("acme", "note", GovernanceMode::Governed);
    let outcome = block_on(
        f.engine
            .create(&invocation(user_actor(), None), note("other", None)),
    );
    assert!(
        outcome.is_ok(),
        "governance for one company changes nothing for another"
    );
    assert!(f.audit.is_empty());
}

#[test]
fn a_failing_audit_write_fails_the_create_closed() {
    let policy = MemoryPolicyStore::new(8).shared();
    policy.set_mode("acme", "note", GovernanceMode::Governed);
    let guard = Arc::new(spot_bpm_governance::GovernanceGuard::new(
        policy,
        Arc::new(spot_bpm_governance::NoRequests),
        Arc::new(spot_bpm_governance::RoleOutsideProcess),
        Arc::new(spot_bpm_governance::FailingAudit),
    ));
    let controller: Arc<dyn Controller> = Arc::new(GovernanceController::new(
        "note",
        guard,
        principals(plain_principal()),
    ));
    let engine = engine_with(vec![("bpm".to_owned(), controller)]);
    let error = block_on(engine.create(&invocation(user_actor(), None), note("acme", None)))
        .expect_err("the operation fails closed when its audit cannot be written");
    assert_eq!(code(&error), "bpm_governance.audit");
}
