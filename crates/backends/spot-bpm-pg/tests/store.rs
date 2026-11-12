//! The store's own contract on a real database: create/load/find, the version check, the
//! exactly-once outcome guard, variables and the runnable list.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::json;
use spot_bpm_engine::{
    Attempt, AttemptState, CaseId, CaseStore, CaseUpdate, EngineError, TimerRecord,
};
use spot_bpm_pg::PgCaseStore;
use spot_dag_exec::{Instant, NodeOutcome, NodeStatus, RunStatus, claim, complete};
use spot_postgres::testing::TestDb;

async fn store(db: &TestDb) -> PgCaseStore {
    let store = PgCaseStore::new(pool(db, 4, "bpm-store"));
    store.prepare().await.unwrap();
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_round_trips_and_is_found_by_key() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(3);
    let record = new_case(&dag, "case_round", "round-key");
    let stored = store.create(record.clone()).await.unwrap();
    assert_eq!(stored, record);

    let id = CaseId::new("case_round").unwrap();
    let loaded = store.load(&id).await.unwrap().unwrap();
    assert_eq!(loaded, record);
    assert_eq!(
        loaded.run.node(&nid("n0")).unwrap().status,
        NodeStatus::Ready
    );

    let found = store.find_by_key("round-key").await.unwrap().unwrap();
    assert_eq!(found.run.id, id);
    assert!(store.find_by_key("no-such-key").await.unwrap().is_none());

    // A ready node makes the Case runnable; a Case waiting on nothing else is listed.
    let runnable = store.list_runnable(now(), 10).await.unwrap();
    assert!(runnable.contains(&id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_business_key_is_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(2);
    store
        .create(new_case(&dag, "case_dup_a", "same-key"))
        .await
        .unwrap();
    let error = store
        .create(new_case(&dag, "case_dup_b", "same-key"))
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::StoreFailed { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_and_completion_advance_the_version() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(3);
    let id = CaseId::new("case_advance").unwrap();
    let record = store
        .create(new_case(&dag, "case_advance", "advance-key"))
        .await
        .unwrap();
    assert_eq!(record.run.version, 1, "a fresh run starts at version 1");

    let begin = claim(&record.run, &nid("n0"), now()).unwrap();
    let running = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new().with_transition(begin),
        )
        .await
        .unwrap();
    assert_eq!(running.run.version, 2);
    assert_eq!(
        running.run.node(&nid("n0")).unwrap().status,
        NodeStatus::Running
    );

    let done = complete(
        &dag,
        &running.run,
        &nid("n0"),
        NodeOutcome::Done {
            output: Some(json!({ "ok": true })),
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap();
    let after = store
        .commit(
            &id,
            running.run.version,
            CaseUpdate::new().with_transition(done),
        )
        .await
        .unwrap();
    assert_eq!(after.run.version, 3);
    assert_eq!(after.run.node(&nid("n0")).unwrap().status, NodeStatus::Done);
    assert_eq!(
        after.run.node(&nid("n1")).unwrap().status,
        NodeStatus::Ready
    );
    assert_eq!(after.run.status, RunStatus::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_version_writes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(2);
    let id = CaseId::new("case_stale").unwrap();
    let record = store
        .create(new_case(&dag, "case_stale", "stale-key"))
        .await
        .unwrap();
    let result = store
        .commit(&id, record.run.version + 5, CaseUpdate::new())
        .await;
    assert!(matches!(result, Err(EngineError::VersionConflict { .. })));
    let reloaded = store.load(&id).await.unwrap().unwrap();
    assert_eq!(reloaded.run.version, record.run.version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_completion_is_a_no_op() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(3);
    let id = CaseId::new("case_replay").unwrap();
    let record = store
        .create(new_case(&dag, "case_replay", "replay-key"))
        .await
        .unwrap();
    let begin = claim(&record.run, &nid("n0"), now()).unwrap();
    let running = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new().with_transition(begin),
        )
        .await
        .unwrap();
    let done = complete(
        &dag,
        &running.run,
        &nid("n0"),
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap();
    let first = store
        .commit(
            &id,
            running.run.version,
            CaseUpdate::new().with_transition(done.clone()),
        )
        .await
        .unwrap();
    // The same transition against the version it was computed from, after a restart.
    let second = store
        .commit(
            &id,
            running.run.version,
            CaseUpdate::new().with_transition(done),
        )
        .await
        .unwrap();
    assert_eq!(second, first);
    assert_eq!(second.run.version, first.run.version);
    assert_eq!(second.run.node(&nid("n0")).unwrap().attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_outcome_is_never_overwritten() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(2);
    let id = CaseId::new("case_guard").unwrap();
    let record = store
        .create(new_case(&dag, "case_guard", "guard-key"))
        .await
        .unwrap();
    let done = Attempt {
        node: nid("n0"),
        attempt: 1,
        state: AttemptState::Done,
        output: Some(json!({ "value": 7 })),
        branches: Vec::new(),
        failure: None,
        started_at: Instant::from_offset(now()),
        not_before: None,
        is_retry: false,
    };
    let stored = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(done)
                .with_at(Instant::from_offset(now())),
        )
        .await
        .unwrap();
    assert_eq!(
        stored.attempts.get(&nid("n0")).unwrap().state,
        AttemptState::Done
    );

    // A replayed `Started` for the same attempt must not erase the stored terminal outcome.
    let started = Attempt {
        node: nid("n0"),
        attempt: 1,
        state: AttemptState::Started,
        output: None,
        branches: Vec::new(),
        failure: None,
        started_at: Instant::from_offset(now()),
        not_before: None,
        is_retry: true,
    };
    let replayed = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(started)
                .with_at(Instant::from_offset(now())),
        )
        .await
        .unwrap();
    let stored_outcome = replayed.attempts.get(&nid("n0")).unwrap();
    assert_eq!(stored_outcome.state, AttemptState::Done);
    assert_eq!(stored_outcome.output, Some(json!({ "value": 7 })));
    assert_eq!(
        replayed.run.version, record.run.version,
        "no transition, no bump"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn variables_round_trip() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(2);
    let id = CaseId::new("case_vars").unwrap();
    let record = store
        .create(new_case(&dag, "case_vars", "vars-key"))
        .await
        .unwrap();
    assert_eq!(record.variables, json!({}));
    let updated = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new().with_variables(json!({ "order": { "total": 12 } })),
        )
        .await
        .unwrap();
    assert_eq!(updated.variables, json!({ "order": { "total": 12 } }));
    let loaded = store.load(&id).await.unwrap().unwrap();
    assert_eq!(loaded.variables, json!({ "order": { "total": 12 } }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_runnable_reports_due_timers_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let dag = chain(2);

    // A waiting attempt, as the engine stores it alongside its timer.
    let waiting = |due: Option<spot_app_api::prelude::OffsetDateTime>| Attempt {
        node: nid("n0"),
        attempt: 1,
        state: AttemptState::Waiting,
        output: None,
        branches: Vec::new(),
        failure: None,
        started_at: Instant::from_offset(now()),
        not_before: due.map(Instant::from_offset),
        is_retry: false,
    };
    let timer =
        |due: Option<spot_app_api::prelude::OffsetDateTime>, event: Option<&str>| TimerRecord {
            node: nid("n0"),
            attempt: 1,
            due: due.map(Instant::from_offset),
            event: event.map(str::to_owned),
            human: None,
        };

    // Each Case claims its entry node first, so no Case has a ready node and only the timer can
    // make it runnable.
    let claimed =
        |record: &spot_bpm_engine::CaseRecord| claim(&record.run, &nid("n0"), now()).unwrap();

    let due = CaseId::new("case_due").unwrap();
    let future = CaseId::new("case_future").unwrap();
    let waiting_event = CaseId::new("case_event").unwrap();
    let later = now() + time::Duration::seconds(3600);

    let record = store
        .create(new_case(&dag, "case_due", "due-key"))
        .await
        .unwrap();
    let record = store
        .commit(
            &due,
            record.run.version,
            CaseUpdate::new().with_transition(claimed(&record)),
        )
        .await
        .unwrap();
    store
        .commit(
            &due,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(waiting(Some(now())))
                .with_timer(timer(Some(now()), None))
                .with_at(Instant::from_offset(now())),
        )
        .await
        .unwrap();

    let record = store
        .create(new_case(&dag, "case_future", "future-key"))
        .await
        .unwrap();
    let record = store
        .commit(
            &future,
            record.run.version,
            CaseUpdate::new().with_transition(claimed(&record)),
        )
        .await
        .unwrap();
    store
        .commit(
            &future,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(waiting(Some(later)))
                .with_timer(timer(Some(later), None))
                .with_at(Instant::from_offset(now())),
        )
        .await
        .unwrap();

    let runnable = store.list_runnable(now(), 10).await.unwrap();
    assert!(runnable.contains(&due), "a due timer is runnable");
    assert!(
        !runnable.contains(&future),
        "a timer an hour away is not runnable yet"
    );

    // An event wait with no due instant is not runnable until it is signalled.
    let record = store
        .create(new_case(&dag, "case_event", "event-key"))
        .await
        .unwrap();
    let record = store
        .commit(
            &waiting_event,
            record.run.version,
            CaseUpdate::new().with_transition(claimed(&record)),
        )
        .await
        .unwrap();
    store
        .commit(
            &waiting_event,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(waiting(None))
                .with_timer(timer(None, Some("order.approved")))
                .with_at(Instant::from_offset(now())),
        )
        .await
        .unwrap();
    let runnable = store.list_runnable(now(), 10).await.unwrap();
    assert!(!runnable.contains(&waiting_event));

    // The two named lookups find the waits.
    let waiting_on = store.waiting_on_event("order.approved").await.unwrap();
    assert_eq!(waiting_on, vec![(waiting_event.clone(), nid("n0"))]);
    let due_timers = store.due_timers(now()).await.unwrap();
    assert!(due_timers.contains(&(due.clone(), nid("n0"))));
    assert!(!due_timers.iter().any(|(case, _)| *case == future));
    // Another tenant sees none of this tenant's timers.
    let other = store.clone().with_tenant("other-tenant");
    assert!(other.due_timers(now()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_case_is_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db).await;
    let id = CaseId::new("case_missing").unwrap();
    assert!(store.load(&id).await.unwrap().is_none());
    let result = store.commit(&id, 0, CaseUpdate::new()).await;
    assert!(matches!(result, Err(EngineError::CaseNotFound { .. })));
}
