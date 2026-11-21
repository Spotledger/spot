//! Transactional and non-transactional DDL (the documented list): a transactional step is one
//! transaction, a concurrent index build is declared non-transactional and its compensating
//! restore point drops an invalid index.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_core::SystemClock;
use spot_migration_apply::step::{Idempotency, RestorePoint, StepContext, StepRunner};
use spot_migration_apply::{Error as ApplyError, StepOutcome};
use spot_migration_apply_pg::{PgApplySchema, PgStepRunner};
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_table_step_is_transactional_and_has_no_restore_point() {
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let catalog = catalog(2);
    let plan = schema_plan(2);
    let mut runner = PgStepRunner::new(
        backend.clone(),
        site(),
        catalog,
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.unwrap();
    let descriptor = StepRunner::describe(&runner, 0, &plan.steps()[0]);
    assert!(descriptor.transactional);
    assert_eq!(descriptor.idempotency, Some(Idempotency::Idempotent));
    assert!(StepRunner::restore_point(&runner, &descriptor).is_none());
    let ctx = StepContext::new(&site(), plan.plan_digest.clone(), "m", 1_000);
    StepRunner::run(&runner, &descriptor, &ctx).await.unwrap();
    let live = live_branch_tables(&backend).await;
    assert_eq!(live, vec!["branch_1".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_index_build_is_declared_non_transactional_and_compensated() {
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();

    // The table exists and holds rows, so the planner builds its index concurrently.
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE party (id uuid PRIMARY KEY, version bigint NOT NULL, \
             note text COLLATE \"C\")",
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "INSERT INTO party (id, version, note) \
             SELECT gen_random_uuid(), 1, 'x' FROM generate_series(1, 50)",
        )
        .await
        .unwrap();

    let catalog = indexed_catalog(1);
    let plan = schema_plan(1);
    let mut runner = PgStepRunner::new(
        backend.clone(),
        site(),
        catalog,
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.unwrap();
    let descriptor = StepRunner::describe(&runner, 0, &plan.steps()[0]);
    assert!(
        !descriptor.transactional,
        "a concurrent index build must be declared non-transactional"
    );
    let restore_point = StepRunner::restore_point(&runner, &descriptor)
        .expect("a non-transactional step records a restore point");
    assert!(restore_point.id.contains("invalid-indexes"));

    let ctx = StepContext::new(&site(), plan.plan_digest.clone(), "m", 1_000);
    let outcome: StepOutcome = StepRunner::run(&runner, &descriptor, &ctx).await.unwrap();
    assert!(outcome.detail.contains("planner step"));

    // The index exists and is valid.
    let valid: bool = client
        .query_one(
            "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'party_note_idx'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(valid);

    // The compensation drops only an invalid leftover; a valid index survives.
    runner.rollback(&restore_point, &ctx).await.unwrap();
    let still: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'party_note_idx')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        still,
        "a valid index must never be dropped by the compensation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_compensation_drops_an_invalid_index_left_by_a_failed_concurrent_build() {
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let client = db.raw_client().await;
    client
        .batch_execute("CREATE TABLE party (id uuid PRIMARY KEY, note text COLLATE \"C\")")
        .await
        .unwrap();
    client
        .batch_execute("CREATE INDEX CONCURRENTLY party_note_idx ON party (note)")
        .await
        .unwrap();
    // Simulate the leftover of a failed build: the catalogue has no such index, so the
    // planner would drop and rebuild it. The compensation is what removes it.
    client
        .batch_execute(
            "UPDATE pg_index SET indisvalid = false WHERE indexrelid = \
                        'party_note_idx'::regclass",
        )
        .await
        .unwrap();

    let mut runner = PgStepRunner::new(
        backend,
        site(),
        catalog(0),
        vec![],
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.unwrap();
    let dropped = runner.drop_invalid_indexes().await.unwrap();
    assert_eq!(dropped, 1);
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'party_note_idx')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!exists);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_step_outside_the_plan_is_refused_without_touching_the_database() {
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let plan = schema_plan(1);
    let mut runner = PgStepRunner::new(
        backend.clone(),
        site(),
        catalog(1),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.unwrap();
    let descriptor = StepRunner::describe(&runner, 7, &plan.steps()[0]);
    let ctx = StepContext::new(&site(), plan.plan_digest.clone(), "m", 1_000);
    let error = StepRunner::run(&runner, &descriptor, &ctx)
        .await
        .expect_err("step 7 is outside a one-step plan");
    assert!(matches!(error, ApplyError::StepFailed { .. }), "{error:?}");
    assert!(error.to_string().contains("step_out_of_plan"), "{error}");
    assert!(live_branch_tables(&backend).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restore_point_round_trips_through_the_journal_as_a_compensating_point() {
    // The engine journals the restore point it gets; this asserts the value the runner returns
    // is stable and carries the step's index, so a restart can match it.
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let plan = schema_plan(2);
    let mut runner = PgStepRunner::new(
        backend,
        site(),
        catalog(2),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.unwrap();
    // The descriptor of a non-transactional step: the shape the engine stores in the journal.
    let mut descriptor = StepRunner::describe(&runner, 1, &plan.steps()[1]);
    descriptor.transactional = false;
    let point = StepRunner::restore_point(&runner, &descriptor)
        .expect("a non-transactional step records a restore point");
    let json = serde_json::to_value(&point).unwrap();
    let back: RestorePoint = serde_json::from_value(json).unwrap();
    assert_eq!(back, point);
    assert_eq!(
        back.id,
        format!("pg-migrate:invalid-indexes:{}", descriptor.index)
    );
    // The compensating point is what a restart rolls back to; it names only the step.
    assert!(!back.detail.contains("password"));
}
