//! The inbox permission rules: only the assignee set acts, a document reader may only see, the
//! permission check judges the document's real state and values, and a repeated `complete` does
//! not leak or resume for a stranger.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{block_on, now, plus, spec, world};
use spot_app_api::prelude::{RecordId, Value};
use spot_bpm_tasks::{
    Decision::Approve, DelegationRequest, DelegationScope, DocFactsSource, InboxAccess,
    MapDocFacts, MapPrincipals, PermissionAccess, PrincipalSource, Task, TaskService, TasksError,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{AppliesTo, AssignmentContext, Principal, Role, UserPermission};
use spot_permissions::{DocFacts, ModelTexts, PermissionEngine, Snapshot, load};

fn customer(byte: u8) -> RecordId {
    let mut bytes = [byte; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

fn engine() -> Arc<PermissionEngine> {
    let mut customer_def = Definition::new("customer");
    customer_def.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut order = Definition::new("order");
    let mut link = FieldSpec::new("customer", FieldType::Link);
    link.options = Some("customer".to_owned());
    order.fields = vec![link];
    let meta = Registry::new(vec![customer_def, order])
        .unwrap()
        .effective();
    let model = load(&ModelTexts {
        grants: r#"
[[entries]]
role = "Reader"
doctype = "order"
rights = ["read"]
"#
        .to_owned(),
        ..ModelTexts::default()
    })
    .unwrap();
    let snapshot = Snapshot::build(&meta, &model).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// `reader_user` holds Reader unscoped; `east_user` holds Reader scoped to customer 1.
struct Principals;

impl PrincipalSource for Principals {
    fn principal(&self, user: &str) -> Principal {
        match user {
            "reader_user" => MapPrincipals::new()
                .with_roles("reader_user", &["Reader"])
                .principal(user),
            "east_user" => Principal::user(
                "east_user",
                vec![AssignmentContext {
                    id: "a".to_owned(),
                    roles: [Role("Reader".to_owned())].into_iter().collect(),
                    scope: vec![UserPermission {
                        user: String::new(),
                        doctype: None,
                        field_or_link_doctype: "order.customer".to_owned(),
                        allowed: vec![customer(1).to_string()],
                        applies_to: AppliesTo::AllDoctypes,
                        include_descendants: false,
                    }],
                }],
            ),
            _ => Principal::with_roles(user, &[]),
        }
    }
}

fn order_facts(customer_byte: u8) -> DocFacts {
    DocFacts {
        doctype: "order".to_owned(),
        id: Some("order_1".to_owned()),
        created_by: None,
        state: Some("draft".to_owned()),
        values: BTreeMap::from([("customer".to_owned(), Value::Id(customer(customer_byte)))]),
    }
}

fn ordered_task(assignees: &[&str]) -> Task {
    let mut task_spec = spec("t1", "case1", "approve", assignees);
    task_spec.doctype = Some("order".to_owned());
    task_spec.subject = Some("order_1".to_owned());
    Task::open(task_spec)
}

fn access_over(facts: Arc<dyn DocFactsSource>) -> PermissionAccess {
    PermissionAccess::new(
        engine(),
        Arc::new(Principals),
        Arc::new(spot_bpm_tasks::MapDirectory::new()),
        facts,
    )
}

#[test]
fn a_reader_may_see_but_never_act() {
    let access = access_over(Arc::new(
        MapDocFacts::new().with_document("order_1", order_facts(1)),
    ));
    let task = ordered_task(&["alice"]);
    assert!(access.may_act("alice", &task));
    assert!(block_on(access.may_see("alice", &task)));
    assert!(
        block_on(access.may_see("reader_user", &task)),
        "a reader sees the task"
    );
    assert!(!access.may_act("reader_user", &task), "but may not act");
    assert!(!block_on(access.may_see("nobody", &task)));
    assert!(!access.may_act("nobody", &task));
}

#[test]
fn a_value_restriction_is_judged_on_the_documents_real_values() {
    let task = ordered_task(&["alice"]);
    // The same scoped user: customer 1 is in scope, customer 2 is not.
    let inside = access_over(Arc::new(
        MapDocFacts::new().with_document("order_1", order_facts(1)),
    ));
    assert!(block_on(inside.may_see("east_user", &task)));
    let outside = access_over(Arc::new(
        MapDocFacts::new().with_document("order_1", order_facts(2)),
    ));
    assert!(
        !block_on(outside.may_see("east_user", &task)),
        "a document outside the user's scope is not seen"
    );
}

#[test]
fn a_source_that_cannot_load_the_document_denies() {
    let access = access_over(Arc::new(MapDocFacts::new()));
    let task = ordered_task(&["alice"]);
    assert!(!block_on(access.may_see("reader_user", &task)));
    // The assignee still sees and acts: the failure only narrows the permission route.
    assert!(block_on(access.may_see("alice", &task)));
}

#[test]
fn facts_of_another_document_deny() {
    let mut wrong = order_facts(1);
    wrong.id = Some("order_2".to_owned());
    let access = access_over(Arc::new(MapDocFacts::new().with_document("order_1", wrong)));
    assert!(!block_on(
        access.may_see("reader_user", &ordered_task(&["alice"]))
    ));
}

fn service_with_reader_access() -> (common::World, Arc<TaskService>) {
    let w = world();
    let access = Arc::new(PermissionAccess::new(
        engine(),
        Arc::new(Principals),
        w.directory.clone(),
        Arc::new(MapDocFacts::new().with_document("order_1", order_facts(1))),
    ));
    let service = Arc::new(
        TaskService::new(w.store.clone(), w.engine.clone())
            .with_directory(w.directory.clone())
            .with_access(access),
    );
    (w, service)
}

#[test]
fn a_reader_who_is_not_an_assignee_cannot_claim_complete_or_delegate() {
    let (_w, service) = service_with_reader_access();
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.doctype = Some("order".to_owned());
    task_spec.subject = Some("order_1".to_owned());
    let task = service.open_task(task_spec).unwrap();

    assert!(matches!(
        service.claim(&task.id, "reader_user", now()),
        Err(TasksError::NotAssignee { .. })
    ));
    assert!(matches!(
        block_on(service.complete(&task.id, "reader_user", Approve, None, now())),
        Err(TasksError::NotAssignee { .. })
    ));
    let request = DelegationRequest {
        to: "reader_user".to_owned(),
        scope: DelegationScope::All,
        expires_at: None,
        reason: None,
    };
    assert!(matches!(
        block_on(service.delegate(&task.id, "reader_user", request, now())),
        Err(TasksError::NotAssignee { .. })
    ));
}

#[test]
fn a_reader_sees_the_task_in_listings_but_it_is_not_theirs_to_act_on() {
    let (_w, service) = service_with_reader_access();
    let mut task_spec = spec("t1", "case1", "approve", &["alice"]);
    task_spec.doctype = Some("order".to_owned());
    task_spec.subject = Some("order_1".to_owned());
    service.open_task(task_spec).unwrap();
    assert_eq!(
        block_on(service.my_tasks("reader_user", now()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(block_on(service.my_tasks("alice", now())).unwrap().len(), 1);
    assert!(
        block_on(service.my_tasks("nobody", now()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_repeated_complete_by_a_stranger_is_refused_and_does_not_resume() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    block_on(w.service.complete(&task.id, "alice", Approve, None, now())).unwrap();
    assert_eq!(w.engine.count(), 1);

    // A stranger gets neither the task nor an outcome, for the same or another decision.
    for decision in [Approve, spot_bpm_tasks::Decision::Reject] {
        let refused = block_on(
            w.service
                .complete(&task.id, "stranger", decision, None, plus(1)),
        );
        assert!(
            matches!(refused, Err(TasksError::NotAssignee { .. })),
            "{refused:?}"
        );
    }
    assert_eq!(w.engine.count(), 1);

    // The completer's repeat is still idempotent, and a supervisor may also see the outcome.
    let again = block_on(
        w.service
            .complete(&task.id, "alice", Approve, None, plus(2)),
    )
    .unwrap();
    assert_eq!(again.outcome, Some(Approve));
    assert!(block_on(w.service.complete(&task.id, "mgr", Approve, None, plus(3))).is_ok());
    assert_eq!(w.engine.count(), 1);
}

/// An engine that refuses the first signal, then records the rest.
struct FlakyEngine {
    failed: std::sync::atomic::AtomicBool,
    inner: spot_bpm_tasks::RecordingEngine,
}

impl spot_bpm_tasks::TaskEngine for FlakyEngine {
    fn signal<'a>(
        &'a self,
        case: &'a spot_bpm_engine::CaseId,
        node: &'a str,
        payload: Option<serde_json::Value>,
        at: spot_app_api::prelude::OffsetDateTime,
    ) -> spot_bpm_tasks::TaskFuture<'a, ()> {
        Box::pin(async move {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(TasksError::EngineFailed {
                    detail: "down".to_owned(),
                });
            }
            self.inner.signal(case, node, payload, at).await
        })
    }
}

#[test]
fn a_stranger_cannot_trigger_the_resume_of_a_completed_but_unresumed_task() {
    // The engine is down at the first completion, so the task is Completed and unresumed.
    let w = world();
    let engine = Arc::new(FlakyEngine {
        failed: std::sync::atomic::AtomicBool::new(false),
        inner: spot_bpm_tasks::RecordingEngine::new(),
    });
    let service =
        TaskService::new(w.store.clone(), engine.clone()).with_directory(w.directory.clone());
    let task = service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    let first = block_on(service.complete(&task.id, "alice", Approve, None, now()));
    assert!(first.is_err(), "the signal failed");
    assert_eq!(engine.inner.count(), 0);

    let refused = block_on(service.complete(&task.id, "stranger", Approve, None, plus(1)));
    assert!(matches!(refused, Err(TasksError::NotAssignee { .. })));
    assert_eq!(
        engine.inner.count(),
        0,
        "a stranger did not resume the Case"
    );

    // The completer's retry does resume it, exactly once.
    block_on(service.complete(&task.id, "alice", Approve, None, plus(2))).unwrap();
    assert_eq!(engine.inner.count(), 1);
}
