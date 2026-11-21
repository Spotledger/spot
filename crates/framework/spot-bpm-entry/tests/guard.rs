//! The `process_only` guard at the document engine's pre-create hook: every channel that
//! creates a record runs it, because they all run the record's own create lifecycle. There is
//! no bypass.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::context::DocumentService;
use spot_bpm_entry::{
    CreateRoute, EntryProcesses, ModeAdmin, ProcessOnlyGuard, ScopedPolicyStore,
    generate_entry_process, scope_chain,
};
use spot_bpm_governance::{
    AuditKind, GovernanceController, GovernanceGuard, GovernanceMode, MemoryPrincipals, NoRequests,
    OUTSIDE_PROCESS_PERMISSION, PolicyStore, RoleOutsideProcess,
};
use spot_permissions::{AssignmentContext, Principal};

fn code(error: &spot_app_api::AppError) -> &str {
    &error.code
}

#[test]
fn a_free_create_is_allowed_through_every_channel() {
    let ports = Ports::new();
    let engine = ports.engine();
    for actor in [user_actor(), import_actor(), script_actor()] {
        let outcome = block_on(engine.create(&invocation(actor, None), employee("acme", None)));
        assert!(outcome.is_ok(), "free is the default: {outcome:?}");
    }
    assert!(ports.audit.is_empty());
}

#[test]
fn process_only_refuses_a_direct_create_from_api_import_and_script() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    for actor in [user_actor(), import_actor(), script_actor()] {
        let error = block_on(engine.create(&invocation(actor, None), employee("acme", None)))
            .expect_err("a process_only create outside its entry process is refused");
        assert_eq!(code(&error), "bpm_entry.start_process");
        // The refusal tells the caller which process to start.
        assert_eq!(
            error.details.get("process").and_then(|v| v.as_str()),
            Some("entry_employee")
        );
        assert_eq!(
            error.details.get("record_type").and_then(|v| v.as_str()),
            Some("employee")
        );
    }
    assert_eq!(ports.audit.len(), 3, "every refusal is audited");
    assert!(
        ports
            .audit
            .entries()
            .iter()
            .all(|e| e.kind == AuditKind::Refused)
    );
}

#[test]
fn process_only_accepts_a_create_through_the_entry_process() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    ports.requests.open("req1", "entry_employee");
    ports.requests.allow("req1", "enter", "employee");
    let key = ports.grant(&user_actor(), "req1", "enter", 1);
    let outcome = block_on(engine.create(
        &invocation(user_actor(), Some(&key)),
        employee("acme", Some("req1")),
    ));
    assert!(
        outcome.is_ok(),
        "the request's own step may create: {outcome:?}"
    );
    assert!(ports.audit.is_empty());
}

#[test]
fn a_request_of_another_process_or_node_does_not_count() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    // A request of some other process, at a node that is allowed to create.
    ports.requests.open("other1", "purchase_flow");
    ports.requests.allow("other1", "enter", "employee");
    // The right process, but a node that may not create the type.
    ports.requests.open("req2", "entry_employee");
    let keys = [
        ports.grant(&user_actor(), "other1", "enter", 1),
        ports.grant(&user_actor(), "req2", "review", 1),
        // A grant for a request nobody opened.
        ports.grant(&user_actor(), "ghost", "enter", 1),
    ];
    for key in keys {
        let error = block_on(engine.create(
            &invocation(user_actor(), Some(&key)),
            employee("acme", None),
        ))
        .expect_err("not a valid request context for the entry process");
        assert_eq!(code(&error), "bpm_entry.start_process", "{key}");
    }
}

#[test]
fn the_outside_process_permission_is_ignored_in_process_only() {
    // Governance and the entry guard both sit on the type; the user holds the outside-process
    // permission and supplies a reason. In `governed` mode that would be an audited exception;
    // in `process_only` it changes nothing.
    let ports = Ports::new();
    let governance = Arc::new(GovernanceGuard::new(
        ports.modes.clone(),
        Arc::new(NoRequests),
        Arc::new(RoleOutsideProcess),
        ports.audit.clone(),
    ));
    let principals = MemoryPrincipals::new();
    principals.insert(
        user_id(),
        Principal::user(
            &user_id().to_string(),
            vec![AssignmentContext::unscoped(
                "default",
                &[OUTSIDE_PROCESS_PERMISSION],
            )],
        ),
    );
    let governance_controller: Arc<dyn spot_app_api::Controller> = Arc::new(
        GovernanceController::new("employee", governance, Arc::new(principals)),
    );
    let entry_controller: Arc<dyn spot_app_api::Controller> = Arc::new(
        spot_bpm_entry::ProcessOnlyController::new("employee", ports.guard(), ports.scopes.clone()),
    );
    let engine = engine_with(vec![governance_controller, entry_controller]);
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    let mut doc = employee("acme", None);
    doc.set(
        n("outside_process_reason"),
        spot_app_api::prelude::Value::Text("urgent hire".into()),
    );
    let error = block_on(engine.create(&invocation(user_actor(), None), doc))
        .expect_err("no bypass in process_only");
    assert_eq!(code(&error), "bpm_entry.start_process");
}

#[test]
fn the_guard_has_no_permission_route_by_construction() {
    // `ProcessOnlyGuard::new` takes the mode source, the entry processes, the requests and the
    // audit sink: no principal source and no permission port, so there is nothing to bypass
    // with. This compiles only while that stays true.
    let ports = Ports::new();
    let _guard: ProcessOnlyGuard = ProcessOnlyGuard::new(
        ports.modes.clone(),
        Arc::new(EntryProcesses::new(ports.settings.clone())),
        ports.requests.clone(),
        ports.audit.clone(),
    );
}

#[test]
fn the_mode_narrows_by_scope_and_most_specific_wins() {
    let store = ScopedPolicyStore::new(8);
    store.seed("acme", "employee", GovernanceMode::ProcessOnly);
    store.seed("acme/north/shop1", "employee", GovernanceMode::Free);
    store.seed("acme/south", "item", GovernanceMode::Governed);
    let at = now();
    assert_eq!(
        store.mode_at("acme", "employee", at).unwrap(),
        GovernanceMode::ProcessOnly
    );
    assert_eq!(
        store.mode_at("acme/north", "employee", at).unwrap(),
        GovernanceMode::ProcessOnly
    );
    assert_eq!(
        store.mode_at("acme/north/shop2", "employee", at).unwrap(),
        GovernanceMode::ProcessOnly
    );
    assert_eq!(
        store.mode_at("acme/north/shop1", "employee", at).unwrap(),
        GovernanceMode::Free,
        "a deeper unit narrows the mode back"
    );
    assert_eq!(
        store.mode_at("acme/south", "item", at).unwrap(),
        GovernanceMode::Governed
    );
    assert_eq!(
        store.mode_at("other", "employee", at).unwrap(),
        GovernanceMode::Free
    );
    assert_eq!(scope_chain("a/b/c"), vec!["a/b/c", "a/b", "a"]);
}

#[test]
fn a_mode_change_is_effective_dated_and_audited() {
    let ports = Ports::new();
    let admin = ModeAdmin::new(ports.modes.clone(), ports.audit.clone());
    admin
        .set_mode(
            "acme",
            "employee",
            GovernanceMode::ProcessOnly,
            later(3600),
            &user_actor(),
            now(),
        )
        .unwrap();
    let entries = ports.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, AuditKind::PolicyChange);
    // Records created before the effective instant keep the earlier mode.
    assert_eq!(
        ports.modes.mode_at("acme", "employee", now()).unwrap(),
        GovernanceMode::Free
    );
    assert_eq!(
        ports
            .modes
            .mode_at("acme", "employee", later(3600))
            .unwrap(),
        GovernanceMode::ProcessOnly
    );
    let engine = ports.engine();
    assert!(
        block_on(engine.create(&invocation(user_actor(), None), employee("acme", None))).is_ok()
    );
}

#[test]
fn a_failing_audit_fails_the_refusal_closed() {
    let ports = Ports::new();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    let guard = Arc::new(ProcessOnlyGuard::new(
        ports.modes.clone(),
        Arc::new(EntryProcesses::new(ports.settings.clone())),
        ports.requests.clone(),
        Arc::new(spot_bpm_governance::FailingAudit),
    ));
    let controller: Arc<dyn spot_app_api::Controller> = Arc::new(
        spot_bpm_entry::ProcessOnlyController::new("employee", guard, ports.scopes.clone()),
    );
    let engine = engine_with(vec![controller]);
    let error = block_on(engine.create(&invocation(user_actor(), None), employee("acme", None)))
        .expect_err("refused");
    assert_eq!(code(&error), "bpm_entry.audit");
}

#[test]
fn the_entry_process_comes_from_the_setting_or_is_generated() {
    let ports = Ports::new();
    let entries = EntryProcesses::new(ports.settings.clone());
    let generated = entries.entry_process("employee", "acme").unwrap();
    assert_eq!(generated.id, "entry_employee");
    assert!(generated.generated);

    ports
        .settings
        .set_entry("employee", "acme", "hire_employee");
    ports
        .settings
        .set_entry("employee", "acme/north", "hire_north");
    assert_eq!(
        entries.entry_process("employee", "acme").unwrap().id,
        "hire_employee"
    );
    assert_eq!(
        entries
            .entry_process("employee", "acme/north/s1")
            .unwrap()
            .id,
        "hire_north"
    );
    assert_eq!(
        entries.entry_process("item", "acme").unwrap().id,
        "entry_item"
    );

    ports.settings.set_entry("employee", "bad", "Not A Process");
    assert!(entries.entry_process("employee", "bad").is_err());
}

#[test]
fn the_route_sends_imports_and_scripts_to_the_entry_process() {
    let ports = Ports::new();
    let guard = ports.guard();
    assert_eq!(
        guard.route("acme", "employee", now()).unwrap(),
        CreateRoute::Direct
    );
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    match guard.route("acme", "employee", now()).unwrap() {
        CreateRoute::StartProcess(process) => assert_eq!(process.id, "entry_employee"),
        CreateRoute::Direct => panic!("a process_only scope must start the entry process"),
    }
}

#[test]
fn the_generated_entry_process_is_a_valid_minimal_process() {
    let version = generate_entry_process("employee").unwrap();
    assert_eq!(version.name, "entry_employee");
    let compiled = spot_bpm_model::compile(&version).expect("the generated process compiles");
    assert_eq!(compiled.dag.nodes().len(), 3);
    let kinds: Vec<&str> = version.nodes.iter().map(|n| n.kind.name()).collect();
    assert_eq!(kinds, vec!["trigger", "form", "document"]);
    // The text form round-trips, so a pack can ship or override it.
    let text = spot_bpm_model::to_text(&version);
    assert_eq!(spot_bpm_model::from_text(&text).unwrap(), version);
}

/// The request context is an authenticated grant, not a string a caller can write.
#[test]
fn a_forged_request_key_does_not_skip_process_only() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    ports.requests.open("req1", "entry_employee");
    ports.requests.allow("req1", "enter", "employee");

    // The shapes the old context accepted, written by an external caller.
    for forged in [
        "req1:enter:0",
        "req1:enter:1",
        "req1:enter:99",
        "grant-1",
        "grant-2",
    ] {
        for actor in [user_actor(), import_actor(), script_actor()] {
            let error =
                block_on(engine.create(&invocation(actor, Some(forged)), employee("acme", None)))
                    .expect_err("a command id someone typed is not a request context");
            assert_eq!(code(&error), "bpm_entry.start_process", "{forged}");
        }
    }
}

#[test]
fn a_grant_issued_to_another_person_is_refused() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    ports.requests.open("req1", "entry_employee");
    ports.requests.allow("req1", "enter", "employee");
    // The step was assigned to the import component; a user who copied its key is not it.
    let key = ports.grant(&import_actor(), "req1", "enter", 1);
    let error = block_on(engine.create(
        &invocation(user_actor(), Some(&key)),
        employee("acme", None),
    ))
    .expect_err("the grant is bound to the acting person");
    assert_eq!(code(&error), "bpm_entry.start_process");
    // The person it was issued to is accepted.
    assert!(
        block_on(engine.create(
            &invocation(import_actor(), Some(&key)),
            employee("acme", None)
        ))
        .is_ok()
    );
}

#[test]
fn a_replayed_old_attempt_is_refused() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    ports.requests.open("req1", "entry_employee");
    ports.requests.allow("req1", "enter", "employee");
    let first = ports.grant(&user_actor(), "req1", "enter", 1);
    let second = ports.grant(&user_actor(), "req1", "enter", 2);
    let replay = block_on(engine.create(
        &invocation(user_actor(), Some(&first)),
        employee("acme", None),
    ))
    .expect_err("attempt 1 is over once attempt 2 is issued");
    assert_eq!(code(&replay), "bpm_entry.start_process");
    assert!(
        block_on(engine.create(
            &invocation(user_actor(), Some(&second)),
            employee("acme", None)
        ))
        .is_ok()
    );
}

/// The scope is judged where the creator cannot choose it.
#[test]
fn a_create_naming_another_or_unknown_company_is_refused() {
    let ports = Ports::new();
    let engine = ports.engine();
    ports
        .modes
        .seed("acme", "employee", GovernanceMode::ProcessOnly);
    for company in ["nowhere", "other", "acme-free", "../acme"] {
        let error =
            block_on(engine.create(&invocation(user_actor(), None), employee(company, None)))
                .expect_err("an unknown scope, or one the person has no right to, is refused");
        assert_eq!(code(&error), "bpm_entry.scope_invalid", "{company}");
    }
    // A person with no scope at all cannot create in the type.
    let stranger = spot_app_api::context::Actor::User {
        id: spot_app_api::prelude::RecordId::new_v7(),
        via: None,
    };
    let error = block_on(engine.create(&invocation(stranger, None), employee("acme", None)))
        .expect_err("not assigned to any company");
    assert_eq!(code(&error), "bpm_entry.scope_invalid");
    // A child unit of the person's company inherits its process_only mode.
    let error = block_on(engine.create(
        &invocation(user_actor(), None),
        employee("acme/north", None),
    ))
    .expect_err("process_only is inherited by units below");
    assert_eq!(code(&error), "bpm_entry.start_process");
    // No company named: the person's own scope applies, and it is process_only.
    let mut doc = employee("acme", None);
    doc.set(n("company"), spot_app_api::prelude::Value::Null);
    let error = block_on(engine.create(&invocation(user_actor(), None), doc))
        .expect_err("the default scope is process_only");
    assert_eq!(code(&error), "bpm_entry.start_process");
}
