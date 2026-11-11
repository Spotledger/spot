//! The sync-to-async bridge under load, and the races the single-statement commit must win.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::*;
use spot_bpm_tasks::{TaskFilter, TaskId, TaskStatus, TaskStore, TasksError};

#[test]
fn two_concurrent_commits_have_exactly_one_winner_and_the_loser_leaves_no_event() {
    let Some(fx) = fixture() else { return };
    for round in 0..20 {
        let (_bridge, store) = open_store(fx.db.config(), &format!("race_{round}"));
        let t = task("t_race", "case1", "approve", &["alice", "bob"]);
        let stored = store.create(t.clone(), created(&t)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["alice", "bob"]
            .into_iter()
            .map(|who| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let mut mine = stored.clone();
                mine.status = TaskStatus::Claimed;
                mine.claimed_by = Some(who.to_owned());
                let id = t.id.clone();
                let event = claimed(&t, who, 1);
                thread::spawn(move || {
                    barrier.wait();
                    store.commit(&id, 1, mine, Some(event))
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
        let after = store.load(&t.id).unwrap().unwrap();
        assert_eq!(after.version, 2);
        assert_eq!(after.claimed_by, winner.claimed_by);
        let trail = store.events(&t.id).unwrap();
        assert_eq!(
            trail.len(),
            2,
            "created + the winner's event, nothing from the loser"
        );
        assert_eq!(trail[1].actor, winner.claimed_by.clone().unwrap());
        assert_eq!(trail[1].seq, 2);
    }
}

#[test]
fn many_threads_creating_one_node_make_one_task_and_one_event() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                // Each caller proposes a different id for the same (case, node).
                let t = task(&format!("t_dup_{i}"), "case1", "approve", &["alice"]);
                barrier.wait();
                store.create(t.clone(), created(&t))
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .collect();
    let winner = &results[0].id;
    assert!(
        results.iter().all(|t| &t.id == winner),
        "every caller sees the same task"
    );
    assert_eq!(store.list(&TaskFilter::default()).unwrap().len(), 1);
    assert_eq!(store.events(winner).unwrap().len(), 1);
}

#[test]
fn many_threads_commit_a_chain_without_losing_or_duplicating_an_event() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let t = task("t_chain", "case1", "approve", &["alice"]);
    store.create(t.clone(), created(&t)).unwrap();
    // Eight threads each retry on conflict until they have landed 5 events: 40 in all.
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let store = Arc::clone(&store);
            let id = t.id.clone();
            let t = t.clone();
            thread::spawn(move || {
                let mut landed = 0;
                while landed < 5 {
                    let current = store.load(&id).unwrap().unwrap();
                    let version = current.version;
                    match store.commit(
                        &id,
                        version,
                        current,
                        Some(claimed(&t, &format!("user{n}"), 1)),
                    ) {
                        Ok(_) => landed += 1,
                        Err(TasksError::VersionConflict { .. }) => {}
                        Err(other) => panic!("{other:?}"),
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let trail = store.events(&t.id).unwrap();
    assert_eq!(trail.len(), 41);
    assert_eq!(
        trail.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=41).collect::<Vec<u64>>(),
        "sequence numbers are gapless and unique"
    );
    assert_eq!(store.load(&t.id).unwrap().unwrap().version, 41);
}

#[test]
fn concurrent_readers_and_writers_share_the_bridge() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let handles: Vec<_> = (0..24)
        .map(|i| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let t = task(&format!("t_m{i}"), &format!("case{i}"), "n", &["alice"]);
                store.create(t.clone(), created(&t)).unwrap();
                assert!(store.load(&t.id).unwrap().is_some());
                assert!(!store.list(&TaskFilter::default()).unwrap().is_empty());
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(store.list(&TaskFilter::default()).unwrap().len(), 24);
}

/// A call from a runtime thread is refused with a clear `StoreFailed`, never run and never a
/// deadlock, whatever the runtime flavour.
#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let id = TaskId::new("t_none").unwrap();

    let current = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let s = Arc::clone(&store);
    let i = id.clone();
    current.block_on(async move {
        check(s.load(&i));
        let t = task("t_in", "case1", "n", &["a"]);
        check(s.create(t.clone(), created(&t)));
    });

    let s = Arc::clone(&store);
    let i = id.clone();
    fx.rt.block_on(async move {
        check(s.load(&i));
        check(s.events(&i));
        check(s.list(&TaskFilter::default()));
        // also from the blocking pool
        let s2 = Arc::clone(&s);
        let i2 = i.clone();
        tokio::task::spawn_blocking(move || check(s2.find_by_node("c", "n")))
            .await
            .unwrap();
        drop(i2);
    });
    // Nothing was written by the refused create.
    assert!(store.find_by_node("case1", "n").unwrap().is_none());
}

fn check<T: std::fmt::Debug>(result: Result<T, TasksError>) {
    match result {
        Err(TasksError::StoreFailed { detail }) => {
            assert!(detail.contains("pg_bridge.wrong_thread"), "{detail}");
            assert!(detail.contains("pg task store"), "{detail}");
        }
        other => panic!("expected StoreFailed, got {other:?}"),
    }
}
