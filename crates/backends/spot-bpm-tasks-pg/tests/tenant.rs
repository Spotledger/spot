//! Cross-tenant access: a task and an event written for tenant A are invisible to tenant B in
//! every method of the store, and tenant B cannot change or shadow them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_tasks::{TaskFilter, TaskStatus, TaskStore, TasksError};

#[test]
fn tenants_never_see_each_other() {
    let Some(fx) = fixture() else { return };
    let (bridge, a) = open_store(fx.db.config(), "tenant_a");
    let b = sibling(&bridge, fx.db.config(), "tenant_b");
    assert_eq!(a.tenant(), "tenant_a");
    assert_eq!(b.tenant(), "tenant_b");

    let mut secret = task_with("t_x", "case1", "approve", &["alice"], &["manager"]);
    secret.due = Some(plus(5));
    a.create(secret.clone(), created(&secret)).unwrap();
    let stored = a.load(&secret.id).unwrap().unwrap();
    let mut claim = stored.clone();
    claim.status = TaskStatus::Claimed;
    a.commit(&secret.id, 1, claim, Some(claimed(&secret, "alice", 1)))
        .unwrap();

    // Every read method of B finds nothing of A.
    assert!(b.load(&secret.id).unwrap().is_none());
    assert!(b.find_by_node("case1", "approve").unwrap().is_none());
    assert!(b.events(&secret.id).unwrap().is_empty());
    assert!(b.list(&TaskFilter::default()).unwrap().is_empty());
    let narrow = TaskFilter {
        assignees: Some(vec!["alice".to_owned()]),
        case: Some("case1".to_owned()),
        due_before: Some(plus(1000)),
        status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
        process: Some("approval".to_owned()),
    };
    assert!(b.list(&narrow).unwrap().is_empty());
    assert!(b.list_by_role(&["manager".to_owned()]).unwrap().is_empty());

    // B cannot change A's task: it does not exist for B, and nothing of A moved.
    let mut hijack = stored.clone();
    hijack.status = TaskStatus::Completed;
    assert_eq!(
        b.commit(&secret.id, 2, hijack, Some(claimed(&secret, "mallory", 9)))
            .unwrap_err(),
        TasksError::TaskNotFound {
            id: "t_x".to_owned()
        }
    );
    let still = a.load(&secret.id).unwrap().unwrap();
    assert_eq!(still.status, TaskStatus::Claimed);
    assert_eq!(still.version, 2);
    assert_eq!(a.events(&secret.id).unwrap().len(), 2);

    // B can create the same task id and the same (case, node): its own, independent task.
    let mine = task("t_x", "case1", "approve", &["bob"]);
    let made = b.create(mine.clone(), created(&mine)).unwrap();
    assert_eq!(
        made.assignees, mine.assignees,
        "B did not get A's task back"
    );
    assert_eq!(made.version, 1);
    assert_eq!(
        a.load(&secret.id).unwrap().unwrap().version,
        2,
        "A is untouched"
    );
    assert_eq!(b.events(&secret.id).unwrap().len(), 1);
    assert_eq!(a.events(&secret.id).unwrap().len(), 2);

    // Each list holds only its own.
    assert_eq!(a.list(&TaskFilter::default()).unwrap().len(), 1);
    assert_eq!(b.list(&TaskFilter::default()).unwrap().len(), 1);
    assert_eq!(
        b.list(&TaskFilter::default()).unwrap()[0].assignees,
        mine.assignees
    );
}

#[test]
fn the_tenant_name_must_be_usable() {
    let Some(fx) = fixture() else { return };
    let (bridge, pool) = open_bridge(fx.db.config());
    for bad in ["", &"t".repeat(129), "a\nb", "a\u{0}b"] {
        let err = spot_bpm_tasks_pg::PgTaskStore::new(std::sync::Arc::clone(&bridge), &pool, bad)
            .unwrap_err();
        assert_eq!(err.code(), "bpm_tasks_pg.bad_setup");
    }
}
