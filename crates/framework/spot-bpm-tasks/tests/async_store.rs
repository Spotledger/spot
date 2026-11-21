//! The in-memory [`AsyncTaskStore`] meets the same contract as the synchronous reference it wraps:
//! the idempotent create, the version check, the event sequence, `list` filters, and the
//! `dyn`-compatibility a host needs to hold it as an `Arc<dyn AsyncTaskStore>`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{block_on, now, plus, spec};
use spot_bpm_tasks::{
    AsyncInMemoryTaskStore, AsyncTaskStore, InMemoryTaskStore, Task, TaskAction, TaskEvent,
    TaskFilter, TaskId, TaskStatus, TaskStore, TasksError,
};

fn task(id: &str, case: &str, node: &str, assignees: &[&str]) -> Task {
    Task::open(spec(id, case, node, assignees))
}

fn created(task: &Task) -> TaskEvent {
    TaskEvent::new(&task.id, TaskAction::Created, "system:engine", now())
}

fn claimed(task: &Task, by: &str, at: u64) -> TaskEvent {
    TaskEvent::new(&task.id, TaskAction::Claimed, by, plus(at))
}

fn ids(tasks: &[Task]) -> Vec<String> {
    tasks.iter().map(|t| t.id.to_string()).collect()
}

#[test]
fn create_is_idempotent_per_case_and_node_and_writes_one_event() {
    let store = AsyncInMemoryTaskStore::new();
    let a = task("t_a", "case1", "approve", &["alice"]);
    let stored = block_on(store.create(a.clone(), created(&a))).unwrap();
    assert_eq!(stored.version, 1);
    let events = block_on(store.events(&a.id)).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, 1);

    let other = task("t_other", "case1", "approve", &["carol"]);
    let replay = block_on(store.create(other.clone(), created(&other))).unwrap();
    assert_eq!(
        replay.id, a.id,
        "the replay returns the task already stored"
    );
    assert!(block_on(store.load(&other.id)).unwrap().is_none());
    assert!(block_on(store.events(&other.id)).unwrap().is_empty());
}

#[test]
fn load_and_find_by_node() {
    let store = AsyncInMemoryTaskStore::new();
    let a = task("t_a", "case1", "approve", &["alice"]);
    block_on(store.create(a.clone(), created(&a))).unwrap();
    assert_eq!(block_on(store.load(&a.id)).unwrap().unwrap().id, a.id);
    let missing = TaskId::new("t_missing").unwrap();
    assert!(block_on(store.load(&missing)).unwrap().is_none());
    assert_eq!(
        block_on(store.find_by_node("case1", "approve"))
            .unwrap()
            .unwrap()
            .id,
        a.id
    );
    assert!(
        block_on(store.find_by_node("case1", "other"))
            .unwrap()
            .is_none()
    );
    assert!(
        block_on(store.find_by_node("case9", "approve"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn commit_checks_the_version_and_continues_the_event_sequence() {
    let store = AsyncInMemoryTaskStore::new();
    let a = task("t_a", "case1", "approve", &["alice", "bob"]);
    let stored = block_on(store.create(a.clone(), created(&a))).unwrap();

    let mut mine = stored.clone();
    mine.status = TaskStatus::Claimed;
    mine.claimed_by = Some("alice".to_owned());
    let done = block_on(store.commit(&a.id, 1, mine, Some(claimed(&a, "alice", 5)))).unwrap();
    assert_eq!(done.version, 2);
    assert_eq!(done.status, TaskStatus::Claimed);

    // The second of two racing commits loses and writes nothing.
    let mut late = stored.clone();
    late.status = TaskStatus::Claimed;
    late.claimed_by = Some("bob".to_owned());
    let err = block_on(store.commit(&a.id, 1, late, Some(claimed(&a, "bob", 6)))).unwrap_err();
    assert_eq!(
        err,
        TasksError::VersionConflict {
            id: "t_a".to_owned(),
            current: 2,
            expected: 1
        }
    );
    let trail = block_on(store.events(&a.id)).unwrap();
    assert_eq!(trail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);

    // A commit without an event changes the version only.
    let quiet = block_on(store.commit(&a.id, 2, done.clone(), None)).unwrap();
    assert_eq!(quiet.version, 3);
    assert_eq!(block_on(store.events(&a.id)).unwrap().len(), 2);

    let ghost = task("t_ghost", "case9", "n", &["x"]);
    assert_eq!(
        block_on(store.commit(&ghost.id, 1, ghost.clone(), None)).unwrap_err(),
        TasksError::TaskNotFound {
            id: "t_ghost".to_owned()
        }
    );
}

#[test]
fn list_filters_and_sorts_by_id() {
    let store = AsyncInMemoryTaskStore::new();
    let mut b = task("t_b", "case2", "approve", &["bob"]);
    b.due = Some(plus(10));
    let c = task("t_c", "case2", "review", &["carol"]);
    let a = task("t_a", "case1", "approve", &["alice"]);
    for t in [&b, &c, &a] {
        block_on(store.create(t.clone(), created(t))).unwrap();
    }
    let all = block_on(store.list(&TaskFilter::default())).unwrap();
    assert_eq!(ids(&all), vec!["t_a", "t_b", "t_c"]);
    let by_case = TaskFilter {
        case: Some("case2".to_owned()),
        ..TaskFilter::default()
    };
    assert_eq!(
        ids(&block_on(store.list(&by_case)).unwrap()),
        vec!["t_b", "t_c"]
    );
    let overdue = TaskFilter {
        due_before: Some(plus(10)),
        ..TaskFilter::default()
    };
    assert_eq!(ids(&block_on(store.list(&overdue)).unwrap()), vec!["t_b"]);
    let none = TaskFilter {
        status: Some(vec![]),
        ..TaskFilter::default()
    };
    assert!(block_on(store.list(&none)).unwrap().is_empty());
}

#[test]
fn the_twin_shares_its_state_with_the_sync_store_it_wraps() {
    let sync = InMemoryTaskStore::new();
    let twin = AsyncInMemoryTaskStore::over(sync.clone());
    let a = task("t_a", "case1", "approve", &["alice"]);
    block_on(twin.create(a.clone(), created(&a))).unwrap();
    // The synchronous view sees it, and the rules are the same code: a replay through the sync
    // store finds the task the twin made.
    assert_eq!(sync.load(&a.id).unwrap().unwrap().id, a.id);
    let b = task("t_b", "case1", "approve", &["bob"]);
    assert_eq!(sync.create(b.clone(), created(&b)).unwrap().id, a.id);
    assert_eq!(twin.sync_store().events(&a.id).unwrap().len(), 1);
}

#[test]
fn the_store_is_usable_as_a_shared_trait_object() {
    let store: Arc<dyn AsyncTaskStore> = Arc::new(AsyncInMemoryTaskStore::new());
    let a = task("t_a", "case1", "approve", &["alice"]);
    let handle = {
        let store = Arc::clone(&store);
        let a = a.clone();
        std::thread::spawn(move || block_on(store.create(a.clone(), created(&a))))
    };
    assert_eq!(handle.join().unwrap().unwrap().id, a.id);
    assert_eq!(block_on(store.load(&a.id)).unwrap().unwrap().id, a.id);
}
