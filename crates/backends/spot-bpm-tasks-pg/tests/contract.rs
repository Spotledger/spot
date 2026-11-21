//! One contract, run against the in-memory reference and the PostgreSQL store: every behaviour
//! of `InMemoryTaskStore` must hold for `PgTaskStore`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_tasks::{
    Decision, InMemoryTaskStore, TaskAction, TaskEvent, TaskFilter, TaskId, TaskStatus, TaskStore,
    TasksError,
};

fn ids(tasks: &[spot_bpm_tasks::Task]) -> Vec<String> {
    tasks.iter().map(|t| t.id.to_string()).collect()
}

/// The shared contract. `fresh` is a store holding nothing for this test's ids.
fn contract(store: &dyn TaskStore) {
    // create: stores at version 1 with event sequence 1.
    let mut a = task_with("t_a", "case1", "approve", &["alice", "bob"], &["manager"]);
    a.due = Some(plus(100));
    let stored = store.create(a.clone(), created(&a)).unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.id, a.id);
    assert_eq!(stored.assignees, a.assignees);
    assert_eq!(stored.roles, a.roles);
    assert_eq!(stored.due, a.due);
    assert_eq!(stored.form, a.form);
    assert_eq!(stored.options, a.options);
    let trail = store.events(&a.id).unwrap();
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].seq, 1);
    assert_eq!(trail[0].action, TaskAction::Created);

    // create is idempotent per (case, node): the stored task comes back, nothing is written.
    let again = task_with("t_other", "case1", "approve", &["carol"], &[]);
    let replay = store.create(again.clone(), created(&again)).unwrap();
    assert_eq!(
        replay.id, a.id,
        "the replay returns the task already stored"
    );
    assert_eq!(replay.assignees, a.assignees);
    assert!(store.load(&again.id).unwrap().is_none());
    assert_eq!(store.events(&a.id).unwrap().len(), 1);
    assert!(store.events(&again.id).unwrap().is_empty());

    // load / find_by_node.
    assert_eq!(store.load(&a.id).unwrap().unwrap().id, a.id);
    assert!(
        store
            .load(&TaskId::new("t_missing").unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.find_by_node("case1", "approve").unwrap().unwrap().id,
        a.id
    );
    assert!(store.find_by_node("case1", "other").unwrap().is_none());
    assert!(store.find_by_node("case9", "approve").unwrap().is_none());

    // commit: version check, update and event together.
    let mut changed = stored.clone();
    changed.status = TaskStatus::Claimed;
    changed.claimed_by = Some("alice".to_owned());
    let committed = store
        .commit(&a.id, 1, changed.clone(), Some(claimed(&a, "alice", 5)))
        .unwrap();
    assert_eq!(committed.version, 2);
    assert_eq!(committed.status, TaskStatus::Claimed);
    assert_eq!(
        store.load(&a.id).unwrap().unwrap().claimed_by.as_deref(),
        Some("alice")
    );
    let trail = store.events(&a.id).unwrap();
    assert_eq!(trail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(trail[1].actor, "alice");

    // a stale version is a conflict and writes nothing.
    let mut stale = committed.clone();
    stale.status = TaskStatus::Completed;
    let err = store
        .commit(&a.id, 1, stale, Some(claimed(&a, "bob", 6)))
        .unwrap_err();
    assert_eq!(
        err,
        TasksError::VersionConflict {
            id: "t_a".to_owned(),
            current: 2,
            expected: 1
        }
    );
    assert_eq!(
        store.load(&a.id).unwrap().unwrap().status,
        TaskStatus::Claimed
    );
    assert_eq!(store.events(&a.id).unwrap().len(), 2);

    // a commit without an event changes the task and the version only.
    let mut quiet = committed.clone();
    quiet.priority = spot_bpm_tasks::Priority::Urgent;
    let quiet = store.commit(&a.id, 2, quiet, None).unwrap();
    assert_eq!(quiet.version, 3);
    assert_eq!(store.events(&a.id).unwrap().len(), 2);
    // and the next event continues the sequence.
    let mut done = quiet.clone();
    done.status = TaskStatus::Completed;
    done.outcome = Some(Decision::Approve);
    let done = store
        .commit(
            &a.id,
            3,
            done,
            Some(
                TaskEvent::new(&a.id, TaskAction::Completed, "alice", plus(9))
                    .with_choice(Decision::Approve, "approve")
                    .with_reason("fine")
                    .on_behalf_of("bob"),
            ),
        )
        .unwrap();
    assert_eq!(done.version, 4);
    let trail = store.events(&a.id).unwrap();
    assert_eq!(
        trail.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(trail[2].choice, Some(Decision::Approve));
    assert_eq!(trail[2].branch.as_deref(), Some("approve"));
    assert_eq!(trail[2].on_behalf_of.as_deref(), Some("bob"));
    assert_eq!(trail[2].reason.as_deref(), Some("fine"));
    assert_eq!(trail[2].at, plus(9));

    // commit of an unknown task.
    let ghost = task("t_ghost", "case9", "n", &["x"]);
    assert_eq!(
        store.commit(&ghost.id, 1, ghost.clone(), None).unwrap_err(),
        TasksError::TaskNotFound {
            id: "t_ghost".to_owned()
        }
    );
    assert!(store.load(&ghost.id).unwrap().is_none());

    // list: a second and third task to filter over.
    let mut b = task_with("t_b", "case2", "approve", &["bob"], &["manager"]);
    b.due = Some(plus(10));
    store.create(b.clone(), created(&b)).unwrap();
    let mut c = task_with("t_c", "case2", "review", &["carol"], &[]);
    c.process = "review".to_owned();
    store.create(c.clone(), created(&c)).unwrap();

    let all = store.list(&TaskFilter::default()).unwrap();
    assert_eq!(ids(&all), vec!["t_a", "t_b", "t_c"], "sorted by id");
    let open = TaskFilter {
        status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&open).unwrap()), vec!["t_b", "t_c"]);
    let completed = TaskFilter {
        status: Some(vec![TaskStatus::Completed]),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&completed).unwrap()), vec!["t_a"]);
    let none_status = TaskFilter {
        status: Some(vec![]),
        ..TaskFilter::default()
    };
    assert!(store.list(&none_status).unwrap().is_empty());
    let by_case = TaskFilter {
        case: Some("case2".to_owned()),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&by_case).unwrap()), vec!["t_b", "t_c"]);
    let by_process = TaskFilter {
        process: Some("review".to_owned()),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&by_process).unwrap()), vec!["t_c"]);
    let by_assignee = TaskFilter {
        assignees: Some(vec!["bob".to_owned(), "nobody".to_owned()]),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&by_assignee).unwrap()), vec!["t_a", "t_b"]);
    let no_assignees = TaskFilter {
        assignees: Some(vec![]),
        ..TaskFilter::default()
    };
    assert!(store.list(&no_assignees).unwrap().is_empty());
    let overdue = TaskFilter {
        due_before: Some(plus(10)),
        ..TaskFilter::default()
    };
    assert_eq!(
        ids(&store.list(&overdue).unwrap()),
        vec!["t_b"],
        "due at the instant counts"
    );
    let overdue_all = TaskFilter {
        due_before: Some(plus(100)),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&overdue_all).unwrap()), vec!["t_a", "t_b"]);
    let combined = TaskFilter {
        status: Some(vec![TaskStatus::Open]),
        case: Some("case2".to_owned()),
        assignees: Some(vec!["carol".to_owned()]),
        due_before: None,
        process: Some("review".to_owned()),
    };
    assert_eq!(ids(&store.list(&combined).unwrap()), vec!["t_c"]);
    let overdue_open = TaskFilter {
        status: Some(vec![TaskStatus::Open]),
        due_before: Some(plus(50)),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&store.list(&overdue_open).unwrap()), vec!["t_b"]);
}

#[test]
fn in_memory_reference_meets_the_contract() {
    contract(&InMemoryTaskStore::new());
}

#[test]
fn postgres_store_meets_the_contract() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    contract(&*store);
}

#[test]
fn role_membership_view_matches_a_scan() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    for (id, case, roles) in [
        ("t_r1", "c1", vec!["manager"]),
        ("t_r2", "c2", vec!["clerk", "manager"]),
        ("t_r3", "c3", vec!["clerk"]),
        ("t_r4", "c4", vec![]),
    ] {
        let t = task_with(id, case, "n", &["u"], &roles);
        store.create(t.clone(), created(&t)).unwrap();
    }
    let by = |roles: &[&str]| {
        let roles: Vec<String> = roles.iter().map(|r| (*r).to_owned()).collect();
        ids(&store.list_by_role(&roles).unwrap())
    };
    assert_eq!(by(&["manager"]), vec!["t_r1", "t_r2"]);
    assert_eq!(by(&["clerk"]), vec!["t_r2", "t_r3"]);
    assert_eq!(by(&["manager", "clerk"]), vec!["t_r1", "t_r2", "t_r3"]);
    assert!(by(&["auditor"]).is_empty());
    assert!(by(&[]).is_empty());
}

#[test]
fn a_restart_keeps_tasks_and_events() {
    let Some(fx) = fixture() else { return };
    let a = task("t_keep", "case1", "approve", &["alice"]);
    {
        let (bridge, store) = open_store(fx.db.config(), "acme");
        let stored = store.create(a.clone(), created(&a)).unwrap();
        let mut changed = stored.clone();
        changed.status = TaskStatus::Claimed;
        store
            .commit(&a.id, 1, changed, Some(claimed(&a, "alice", 3)))
            .unwrap();
        drop(store);
        bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
    }
    // A new bridge, pool and store: the process restarted. prepare runs again and changes nothing.
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let back = store.load(&a.id).unwrap().unwrap();
    assert_eq!(back.status, TaskStatus::Claimed);
    assert_eq!(back.version, 2);
    assert_eq!(
        store.find_by_node("case1", "approve").unwrap().unwrap().id,
        a.id
    );
    let trail = store.events(&a.id).unwrap();
    assert_eq!(trail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    // and the replay of the Decision node still finds the task instead of making a second one.
    let again = task("t_keep2", "case1", "approve", &["zoe"]);
    assert_eq!(
        store.create(again.clone(), created(&again)).unwrap().id,
        a.id
    );
}
