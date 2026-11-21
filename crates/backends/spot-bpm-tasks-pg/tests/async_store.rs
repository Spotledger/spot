//! The async store, driven from tokio runtime threads against a real PostgreSQL: the contract the
//! sync store passes, the one-winner race, the refusal the sync store still gives on a runtime
//! thread (the reason this store exists), a flood of callers beyond the pool, deadlines, tenant
//! isolation and the two stores sharing one set of tables.
//!
//! The scratch database and the sync store's bridge are built on plain threads, outside any
//! runtime; only the calls under test run on the runtime (`rt.block_on` and spawned tasks).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_bpm_tasks::{
    AsyncTaskStore, Decision, Task, TaskAction, TaskEvent, TaskFilter, TaskId, TaskStatus,
    TaskStore, TasksError,
};
use spot_bpm_tasks_pg::AsyncPgTaskStore;
use spot_postgres::PgConfig;
use tokio::sync::Barrier;

fn ids(tasks: &[Task]) -> Vec<String> {
    tasks.iter().map(|t| t.id.to_string()).collect()
}

/// An async store for `tenant` on its own pool of `size`, tables created.
async fn open_async(config: PgConfig, tenant: &str, size: usize) -> Arc<AsyncPgTaskStore> {
    let store = AsyncPgTaskStore::new(pool_of(config, size), tenant).expect("store");
    store.prepare().await.expect("prepare");
    Arc::new(store)
}

fn is_store_failed(error: &TasksError) -> bool {
    matches!(error, TasksError::StoreFailed { .. })
}

#[test]
fn the_async_store_meets_the_contract_from_a_runtime_thread() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        let store = open_async(config, "acme", 4).await;

        // create: version 1, event sequence 1.
        let mut a = task_with("t_a", "case1", "approve", &["alice", "bob"], &["manager"]);
        a.due = Some(plus(100));
        let stored = store.create(a.clone(), created(&a)).await.unwrap();
        assert_eq!(stored.version, 1);
        assert_eq!(stored.assignees, a.assignees);
        assert_eq!(stored.roles, a.roles);
        assert_eq!(stored.due, a.due);
        assert_eq!(stored.form, a.form);
        let trail = store.events(&a.id).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].seq, 1);
        assert_eq!(trail[0].action, TaskAction::Created);

        // create is idempotent per (case, node): the stored task comes back, nothing is written.
        let again = task_with("t_other", "case1", "approve", &["carol"], &[]);
        let replay = store.create(again.clone(), created(&again)).await.unwrap();
        assert_eq!(replay.id, a.id);
        assert!(store.load(&again.id).await.unwrap().is_none());
        assert_eq!(store.events(&a.id).await.unwrap().len(), 1);
        assert!(store.events(&again.id).await.unwrap().is_empty());

        // load / find_by_node.
        assert_eq!(store.load(&a.id).await.unwrap().unwrap().id, a.id);
        let missing = TaskId::new("t_missing").unwrap();
        assert!(store.load(&missing).await.unwrap().is_none());
        assert_eq!(
            store
                .find_by_node("case1", "approve")
                .await
                .unwrap()
                .unwrap()
                .id,
            a.id
        );
        assert!(
            store
                .find_by_node("case1", "other")
                .await
                .unwrap()
                .is_none()
        );

        // commit: version check, update and event together.
        let mut changed = stored.clone();
        changed.status = TaskStatus::Claimed;
        changed.claimed_by = Some("alice".to_owned());
        let committed = store
            .commit(&a.id, 1, changed, Some(claimed(&a, "alice", 5)))
            .await
            .unwrap();
        assert_eq!(committed.version, 2);
        assert_eq!(committed.status, TaskStatus::Claimed);

        // a wrong expected version is a conflict and writes nothing.
        let mut stale = committed.clone();
        stale.status = TaskStatus::Completed;
        let err = store
            .commit(&a.id, 1, stale, Some(claimed(&a, "bob", 6)))
            .await
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
            store.load(&a.id).await.unwrap().unwrap().status,
            TaskStatus::Claimed
        );
        assert_eq!(store.events(&a.id).await.unwrap().len(), 2);

        // a commit without an event changes the task and the version only; the next event
        // continues the sequence, in order.
        let quiet = store
            .commit(&a.id, 2, committed.clone(), None)
            .await
            .unwrap();
        assert_eq!(quiet.version, 3);
        let mut done = quiet.clone();
        done.status = TaskStatus::Completed;
        done.outcome = Some(Decision::Approve);
        store
            .commit(
                &a.id,
                3,
                done,
                Some(
                    TaskEvent::new(&a.id, TaskAction::Completed, "alice", plus(9))
                        .with_choice(Decision::Approve, "approve")
                        .with_reason("fine"),
                ),
            )
            .await
            .unwrap();
        let trail = store.events(&a.id).await.unwrap();
        assert_eq!(
            trail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(trail[2].choice, Some(Decision::Approve));

        // commit of an unknown task.
        let ghost = task("t_ghost", "case9", "n", &["x"]);
        assert_eq!(
            store
                .commit(&ghost.id, 1, ghost.clone(), None)
                .await
                .unwrap_err(),
            TasksError::TaskNotFound {
                id: "t_ghost".to_owned()
            }
        );

        // list: filters and the id order.
        let mut b = task_with("t_b", "case2", "approve", &["bob"], &["manager"]);
        b.due = Some(plus(10));
        store.create(b.clone(), created(&b)).await.unwrap();
        let mut c = task_with("t_c", "case2", "review", &["carol"], &[]);
        c.process = "review".to_owned();
        store.create(c.clone(), created(&c)).await.unwrap();

        let all = store.list(&TaskFilter::default()).await.unwrap();
        assert_eq!(ids(&all), vec!["t_a", "t_b", "t_c"]);
        let open = TaskFilter {
            status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
            ..TaskFilter::default()
        };
        assert_eq!(ids(&store.list(&open).await.unwrap()), vec!["t_b", "t_c"]);
        let by_case = TaskFilter {
            case: Some("case2".to_owned()),
            ..TaskFilter::default()
        };
        assert_eq!(
            ids(&store.list(&by_case).await.unwrap()),
            vec!["t_b", "t_c"]
        );
        let by_process = TaskFilter {
            process: Some("review".to_owned()),
            ..TaskFilter::default()
        };
        assert_eq!(ids(&store.list(&by_process).await.unwrap()), vec!["t_c"]);
        let by_assignee = TaskFilter {
            assignees: Some(vec!["bob".to_owned(), "nobody".to_owned()]),
            ..TaskFilter::default()
        };
        assert_eq!(
            ids(&store.list(&by_assignee).await.unwrap()),
            vec!["t_a", "t_b"]
        );
        let no_assignees = TaskFilter {
            assignees: Some(vec![]),
            ..TaskFilter::default()
        };
        assert!(store.list(&no_assignees).await.unwrap().is_empty());
        let overdue = TaskFilter {
            due_before: Some(plus(10)),
            ..TaskFilter::default()
        };
        assert_eq!(ids(&store.list(&overdue).await.unwrap()), vec!["t_b"]);
        let combined = TaskFilter {
            status: Some(vec![TaskStatus::Open]),
            case: Some("case2".to_owned()),
            assignees: Some(vec!["carol".to_owned()]),
            due_before: None,
            process: Some("review".to_owned()),
        };
        assert_eq!(ids(&store.list(&combined).await.unwrap()), vec!["t_c"]);

        // the by-role view.
        let roles = vec!["manager".to_owned()];
        assert_eq!(
            ids(&store.list_by_role(&roles).await.unwrap()),
            vec!["t_a", "t_b"]
        );
        assert!(store.list_by_role(&[]).await.unwrap().is_empty());
    });
}

#[test]
fn two_tasks_racing_to_complete_one_task_have_one_winner_and_the_loser_leaves_no_event() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        for round in 0..20 {
            let store = open_async(config.clone(), &format!("race_{round}"), 4).await;
            let t = task("t_race", "case1", "approve", &["alice", "bob"]);
            let stored = store.create(t.clone(), created(&t)).await.unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let mut handles = Vec::new();
            for who in ["alice", "bob"] {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let mut mine = stored.clone();
                mine.status = TaskStatus::Claimed;
                mine.claimed_by = Some(who.to_owned());
                let id = t.id.clone();
                let event = claimed(&t, who, 1);
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    store.commit(&id, 1, mine, Some(event)).await
                }));
            }
            let mut results = Vec::new();
            for handle in handles {
                results.push(handle.await.unwrap());
            }
            let winners: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
            assert_eq!(winners.len(), 1, "exactly one commit wins: {results:?}");
            let loser = results.iter().find(|r| r.is_err()).unwrap();
            assert_eq!(
                loser.as_ref().unwrap_err(),
                &TasksError::VersionConflict {
                    id: "t_race".to_owned(),
                    current: 2,
                    expected: 1
                }
            );
            let winner = winners[0].as_ref().unwrap();
            let after = store.load(&t.id).await.unwrap().unwrap();
            assert_eq!(after.version, 2);
            assert_eq!(after.claimed_by, winner.claimed_by);
            let trail = store.events(&t.id).await.unwrap();
            assert_eq!(trail.len(), 2, "created + the winner's event only");
            assert_eq!(trail[1].seq, 2);
            assert_eq!(Some(trail[1].actor.clone()), winner.claimed_by);
        }
    });
}

#[test]
fn many_callers_creating_one_node_make_one_task_and_one_event() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        let store = open_async(config, "acme", 4).await;
        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();
        for i in 0..16 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                let t = task(&format!("t_dup_{i}"), "case1", "approve", &["alice"]);
                barrier.wait().await;
                store.create(t.clone(), created(&t)).await
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap().unwrap());
        }
        let winner = results[0].id.clone();
        assert!(results.iter().all(|t| t.id == winner));
        assert_eq!(store.list(&TaskFilter::default()).await.unwrap().len(), 1);
        assert_eq!(store.events(&winner).await.unwrap().len(), 1);
    });
}

#[test]
fn the_sync_store_is_still_refused_on_a_runtime_thread_and_the_async_one_is_not() {
    let Some(fx) = fixture() else { return };
    // The sync store and its bridge are built here, on a plain thread.
    let (_bridge, sync_store) = open_store(fx.db.config(), "acme");
    let config = fx.db.config();
    let sync_in_runtime = Arc::clone(&sync_store);
    fx.rt.block_on(async move {
        let sync_store = sync_in_runtime;
        let async_store = open_async(config, "acme", 4).await;
        let t = task("t_w", "case1", "approve", &["alice"]);

        // On the thread that drives the runtime, and on a worker thread: the sync store refuses
        // and writes nothing.
        let refused = |result: Result<Option<Task>, TasksError>| match result {
            Err(TasksError::StoreFailed { detail }) => {
                assert!(detail.contains("pg_bridge.wrong_thread"), "{detail}");
            }
            other => panic!("expected the wrong-thread refusal, got {other:?}"),
        };
        refused(sync_store.load(&t.id));
        let on_worker = {
            let sync_store = Arc::clone(&sync_store);
            let id = t.id.clone();
            tokio::spawn(async move { sync_store.load(&id) })
                .await
                .unwrap()
        };
        refused(on_worker);

        // The async store runs on both.
        let stored = async_store.create(t.clone(), created(&t)).await.unwrap();
        assert_eq!(stored.version, 1);
        let worker = {
            let store = Arc::clone(&async_store);
            let id = t.id.clone();
            tokio::spawn(async move { store.load(&id).await })
                .await
                .unwrap()
        };
        assert_eq!(worker.unwrap().unwrap().id, t.id);
    });
    // The refused calls wrote nothing, and the async write is visible to the sync store from a
    // plain thread: the two stores share one set of tables.
    let back = sync_store
        .load(&TaskId::new("t_w").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(back.version, 1);
    assert_eq!(sync_store.events(&back.id).unwrap().len(), 1);
}

#[test]
fn the_two_stores_see_each_others_writes() {
    let Some(fx) = fixture() else { return };
    let (_bridge, sync_store) = open_store(fx.db.config(), "acme");
    let sync_made = task("t_sync", "case1", "a", &["alice"]);
    sync_store
        .create(sync_made.clone(), created(&sync_made))
        .unwrap();
    let config = fx.db.config();
    let async_made = task("t_async", "case1", "b", &["alice"]);
    let made = async_made.clone();
    fx.rt.block_on(async move {
        let store = open_async(config, "acme", 4).await;
        // The async store finds the task the sync store made, replays idempotently, and writes
        // its own.
        assert_eq!(
            store
                .find_by_node("case1", "a")
                .await
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "t_sync"
        );
        let replay = task("t_again", "case1", "a", &["zed"]);
        assert_eq!(
            store
                .create(replay.clone(), created(&replay))
                .await
                .unwrap()
                .id
                .as_str(),
            "t_sync"
        );
        store.create(made.clone(), created(&made)).await.unwrap();
    });
    let list = sync_store.list(&TaskFilter::default()).unwrap();
    assert_eq!(ids(&list), vec!["t_async", "t_sync"]);
}

#[test]
fn many_more_callers_than_the_pool_all_finish_or_fail_cleanly_and_the_pool_recovers() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        // A pool of 3 and 96 concurrent callers mixing every operation.
        let store = open_async(config, "acme", 3).await;
        let mut handles = Vec::new();
        for i in 0..96 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let t = task(&format!("t_{i}"), &format!("case{i}"), "n", &["alice"]);
                let created_task = store.create(t.clone(), created(&t)).await?;
                let mut next = created_task.clone();
                next.status = TaskStatus::Claimed;
                store
                    .commit(&t.id, 1, next, Some(claimed(&t, "alice", 1)))
                    .await?;
                store.events(&t.id).await?;
                store.list(&TaskFilter::default()).await?;
                store.load(&t.id).await
            }));
        }
        let mut done = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(Some(task)) => {
                    assert_eq!(task.status, TaskStatus::Claimed);
                    done += 1;
                }
                Ok(None) => panic!("a created task vanished"),
                // A caller may miss its deadline under load; it must say so, never return
                // a default.
                Err(error) => assert!(is_store_failed(&error), "{error:?}"),
            }
        }
        assert!(done > 0, "some callers complete");
        // The pool recovered: a fresh call works, and every task that completed is there.
        let listed = store.list(&TaskFilter::default()).await.unwrap();
        assert!(listed.len() >= done);
        let probe = task("t_probe", "case_probe", "n", &["alice"]);
        store.create(probe.clone(), created(&probe)).await.unwrap();
        assert!(store.load(&probe.id).await.unwrap().is_some());
    });
}

#[test]
fn a_call_that_cannot_get_a_connection_in_time_fails_with_the_deadline_and_the_pool_recovers() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        let pool = pool_of(config, 1);
        let store = AsyncPgTaskStore::new(pool.clone(), "acme")
            .unwrap()
            .with_deadlines(Duration::from_millis(300), Duration::from_millis(300))
            .unwrap();
        store.prepare().await.unwrap();

        // Hold the pool's only connection: the call waits for it, then misses its deadline.
        let held = pool.get().await.unwrap();
        let t = task("t_late", "case1", "n", &["alice"]);
        let err = store.create(t.clone(), created(&t)).await.unwrap_err();
        match &err {
            TasksError::StoreFailed { detail } => {
                assert!(detail.contains("deadline"), "{detail}");
                assert!(detail.contains("pg task store"), "{detail}");
            }
            other => panic!("expected StoreFailed, got {other:?}"),
        }
        let err = store.load(&t.id).await.unwrap_err();
        assert!(is_store_failed(&err));
        drop(held);

        // The refused create wrote nothing, and the pool serves the next call.
        assert!(store.load(&t.id).await.unwrap().is_none());
        store.create(t.clone(), created(&t)).await.unwrap();
        assert!(store.load(&t.id).await.unwrap().is_some());
    });
}

#[test]
fn a_tenant_never_sees_another_tenants_tasks() {
    let Some(fx) = fixture() else { return };
    let config = fx.db.config();
    fx.rt.block_on(async move {
        let a = open_async(config.clone(), "tenant_a", 3).await;
        let b = open_async(config, "tenant_b", 3).await;
        assert_eq!(a.tenant(), "tenant_a");
        let secret = task_with("t_x", "case1", "approve", &["alice"], &["manager"]);
        a.create(secret.clone(), created(&secret)).await.unwrap();

        assert!(b.load(&secret.id).await.unwrap().is_none());
        assert!(b.find_by_node("case1", "approve").await.unwrap().is_none());
        assert!(b.events(&secret.id).await.unwrap().is_empty());
        assert!(b.list(&TaskFilter::default()).await.unwrap().is_empty());
        assert!(
            b.list_by_role(&["manager".to_owned()])
                .await
                .unwrap()
                .is_empty()
        );

        // B cannot change A's task: it does not exist for B.
        let err = b
            .commit(&secret.id, 1, secret.clone(), None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            TasksError::TaskNotFound {
                id: "t_x".to_owned()
            }
        );
        // And B may create the same (case, node) and id for itself without touching A's.
        let mine = task_with("t_x", "case1", "approve", &["bob"], &[]);
        let made = b.create(mine.clone(), created(&mine)).await.unwrap();
        assert_eq!(made.assignees, mine.assignees);
        let theirs = a.load(&secret.id).await.unwrap().unwrap();
        assert_eq!(theirs.assignees, secret.assignees);
    });
}

#[test]
fn a_bad_setup_is_refused_with_the_setup_code() {
    let Some(fx) = fixture() else { return };
    let pool = pool_of(fx.db.config(), 2);
    let long = "x".repeat(129);
    for tenant in ["", "bad\ntenant", long.as_str()] {
        let err = AsyncPgTaskStore::new(pool.clone(), tenant).unwrap_err();
        assert_eq!(err.code(), spot_bpm_tasks_pg::BAD_SETUP);
    }
    let store = AsyncPgTaskStore::new(pool, "acme").unwrap();
    let err = store
        .with_deadlines(Duration::ZERO, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(err.code(), spot_bpm_tasks_pg::BAD_SETUP);
}
