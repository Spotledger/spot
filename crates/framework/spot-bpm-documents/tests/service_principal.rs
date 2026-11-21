//! A Document node's `principal` is a registered service principal or nothing: a user id is
//! refused, an author needs the publish permission and a grant, and every use is audited.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::{engine, run_node, site};
use serde_json::{Value as Json, json};
use spot_bpm_documents::{
    CaseFileStore, DocumentNodeHandler, EngineDocumentOps, GuardedNode, MemoryCaseFile,
    MemoryDirectory, MemoryUseAudit, ProcessAuthor, PublishGuard, ServicePrincipalPolicy, codes,
    service_principal::PUBLISH_PERMISSION,
};
use spot_bpm_engine::{CaseId, NodeKind, NodeOutcome};
use spot_dag::NodeId;

const SVC: &str = "svc_ledger";

struct World {
    handler: DocumentNodeHandler,
    directory: Arc<MemoryDirectory>,
    audit: Arc<MemoryUseAudit>,
    case_file: Arc<MemoryCaseFile>,
}

fn alice() -> ProcessAuthor {
    ProcessAuthor::User("alice".to_owned())
}

fn world(with_policy: bool) -> World {
    let directory = Arc::new(MemoryDirectory::new());
    let audit = Arc::new(MemoryUseAudit::new());
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(engine(), site(), case_file.clone()));
    let mut handler = DocumentNodeHandler::new(ops);
    if with_policy {
        handler = handler.with_service_principals(ServicePrincipalPolicy::new(
            site(),
            directory.clone(),
            directory.clone(),
            audit.clone(),
        ));
    }
    World {
        handler,
        directory,
        audit,
        case_file,
    }
}

fn config(principal: &str) -> Json {
    json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
        "principal": principal,
    })
}

fn run(world: &World, case: &CaseId, principal: &str) -> NodeOutcome {
    let mut variables = json!({ "title": "SO-1" });
    run_node(
        &world.handler,
        case,
        "order",
        1,
        &config(principal),
        None,
        &mut variables,
    )
}

fn code_of(outcome: &NodeOutcome) -> String {
    match outcome {
        NodeOutcome::Fail { code, .. } => code.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn authorised(world: &World, case: &CaseId) {
    world.directory.register(&site(), SVC);
    world.directory.grant(&site(), SVC, &alice());
    world.directory.set_case_author(&site(), case, &alice());
}

#[test]
fn an_unregistered_name_is_refused() {
    let world = world(true);
    let case = CaseId::new("case_unreg").unwrap();
    world.directory.set_case_author(&site(), &case, &alice());
    let outcome = run(&world, &case, "svc_unknown");
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_REGISTERED);
    assert!(world.case_file.entries(&case).unwrap().is_empty());
    assert!(world.audit.records().is_empty());
}

#[test]
fn a_user_id_is_never_accepted_even_if_registered() {
    let world = world(true);
    let case = CaseId::new("case_uid").unwrap();
    let user = spot_bpm_documents::ids::record_id_for("some-user").to_string();
    world.directory.register(&site(), &user);
    world.directory.grant(&site(), &user, &alice());
    world.directory.set_case_author(&site(), &case, &alice());
    let outcome = run(&world, &case, &user);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_REGISTERED);
    assert!(world.case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn a_node_with_a_principal_is_refused_when_no_registry_is_configured() {
    let world = world(false);
    let case = CaseId::new("case_nopolicy").unwrap();
    let outcome = run(&world, &case, SVC);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_REGISTERED);
}

#[test]
fn a_registered_name_with_an_authorised_author_runs_and_is_audited() {
    let world = world(true);
    let case = CaseId::new("case_ok").unwrap();
    authorised(&world, &case);
    let outcome = run(&world, &case, SVC);
    assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(world.case_file.entries(&case).unwrap().len(), 1);
    let records = world.audit.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].case, case);
    assert_eq!(records[0].node.as_str(), "order");
    assert_eq!(records[0].principal, SVC);
    assert_eq!(records[0].author, alice());
}

#[test]
fn an_author_without_a_grant_is_refused_at_run_time() {
    let world = world(true);
    let case = CaseId::new("case_nogrant").unwrap();
    world.directory.register(&site(), SVC);
    world.directory.set_case_author(&site(), &case, &alice());
    let outcome = run(&world, &case, SVC);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_PERMITTED);
    assert!(world.audit.records().is_empty());
}

#[test]
fn an_unknown_author_is_refused() {
    let world = world(true);
    let case = CaseId::new("case_noauthor").unwrap();
    world.directory.register(&site(), SVC);
    world.directory.grant(&site(), SVC, &alice());
    let outcome = run(&world, &case, SVC);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_PERMITTED);
}

#[test]
fn a_registry_failure_refuses() {
    let world = world(true);
    let case = CaseId::new("case_down").unwrap();
    authorised(&world, &case);
    world.directory.set_unreadable(true);
    let outcome = run(&world, &case, SVC);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_NOT_REGISTERED);
    assert!(world.case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn an_audit_failure_stops_the_node_before_it_acts() {
    let world = world(true);
    let case = CaseId::new("case_audit_down").unwrap();
    authorised(&world, &case);
    world.audit.set_failing(true);
    let outcome = run(&world, &case, SVC);
    assert_eq!(code_of(&outcome), codes::PRINCIPAL_AUDIT_FAILED);
    assert!(world.case_file.entries(&case).unwrap().is_empty());
}

#[test]
fn a_node_without_a_principal_is_unaffected() {
    let world = world(true);
    let case = CaseId::new("case_plain").unwrap();
    let mut variables = json!({ "title": "SO-1" });
    let plain = json!({
        "operation": "create_from",
        "doctype": "sales_order",
        "mapping": { "title": "doc.title" },
    });
    let outcome = run_node(
        &world.handler,
        &case,
        "order",
        1,
        &plain,
        None,
        &mut variables,
    );
    assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
    assert!(world.audit.records().is_empty());
}

fn guard(world: &World) -> PublishGuard {
    PublishGuard::new(world.directory.clone(), world.directory.clone())
}

fn nodes_of<'a>(
    ids: &'a [NodeId],
    kinds: &'a [NodeKind],
    configs: &'a [Json],
) -> Vec<GuardedNode<'a>> {
    (0..ids.len())
        .map(|i| GuardedNode {
            node: &ids[i],
            kind: &kinds[i],
            config: &configs[i],
        })
        .collect()
}

#[test]
fn an_author_without_the_permission_is_refused_at_validation() {
    let world = world(true);
    world.directory.register(&site(), SVC);
    world.directory.grant(&site(), SVC, &alice());
    let ids = [NodeId::new("a").unwrap()];
    let kinds = [NodeKind::Document];
    let configs = [config(SVC)];
    let violations = guard(&world)
        .check(&site(), &alice(), &nodes_of(&ids, &kinds, &configs))
        .unwrap_err();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].code, codes::PRINCIPAL_NOT_PERMITTED);
    assert_eq!(violations[0].node.as_str(), "a");
}

#[test]
fn an_author_with_the_permission_and_a_grant_is_accepted() {
    let world = world(true);
    world.directory.register(&site(), SVC);
    world.directory.grant(&site(), SVC, &alice());
    world
        .directory
        .allow_permission(&site(), &alice(), PUBLISH_PERMISSION);
    let ids = [NodeId::new("a").unwrap()];
    let kinds = [NodeKind::Document];
    let configs = [config(SVC)];
    assert!(
        guard(&world)
            .check(&site(), &alice(), &nodes_of(&ids, &kinds, &configs))
            .is_ok()
    );
}

#[test]
fn the_platform_author_needs_no_permission_but_still_a_registered_principal() {
    let world = world(true);
    world.directory.register(&site(), SVC);
    let ids = [NodeId::new("a").unwrap(), NodeId::new("b").unwrap()];
    let kinds = [NodeKind::Document, NodeKind::Document];
    let configs = [config(SVC), config("svc_other")];
    let violations = guard(&world)
        .check(
            &site(),
            &ProcessAuthor::Platform,
            &nodes_of(&ids, &kinds, &configs),
        )
        .unwrap_err();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].node.as_str(), "b");
    assert_eq!(violations[0].code, codes::PRINCIPAL_NOT_REGISTERED);
}

#[test]
fn every_violation_is_returned_and_other_nodes_are_ignored() {
    let world = world(true);
    world.directory.register(&site(), SVC);
    let user = spot_bpm_documents::ids::record_id_for("u").to_string();
    let ids = [
        NodeId::new("a").unwrap(),
        NodeId::new("b").unwrap(),
        NodeId::new("c").unwrap(),
        NodeId::new("d").unwrap(),
    ];
    let kinds = [
        NodeKind::Document,
        NodeKind::Document,
        NodeKind::Wait,
        NodeKind::Document,
    ];
    let configs = [
        config(SVC),
        config(&user),
        config(SVC),
        json!({ "operation": "map", "doctype": "note" }),
    ];
    let violations = guard(&world)
        .check(&site(), &alice(), &nodes_of(&ids, &kinds, &configs))
        .unwrap_err();
    let found: Vec<(&str, &str)> = violations
        .iter()
        .map(|v| (v.node.as_str(), v.code))
        .collect();
    assert_eq!(
        found,
        vec![
            ("a", codes::PRINCIPAL_NOT_PERMITTED),
            ("a", codes::PRINCIPAL_NOT_PERMITTED),
            ("b", codes::PRINCIPAL_NOT_REGISTERED),
        ]
    );
}

#[test]
fn a_registry_failure_refuses_at_validation() {
    let world = world(true);
    world.directory.register(&site(), SVC);
    world.directory.grant(&site(), SVC, &alice());
    world
        .directory
        .allow_permission(&site(), &alice(), PUBLISH_PERMISSION);
    world.directory.set_unreadable(true);
    let ids = [NodeId::new("a").unwrap()];
    let kinds = [NodeKind::Document];
    let configs = [config(SVC)];
    let violations = guard(&world)
        .check(&site(), &alice(), &nodes_of(&ids, &kinds, &configs))
        .unwrap_err();
    assert_eq!(violations[0].code, codes::PRINCIPAL_NOT_REGISTERED);
}
