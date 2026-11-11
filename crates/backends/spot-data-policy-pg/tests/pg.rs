//! PostgreSQL-specific behaviour: concurrency, rollback, restart, tenant scoping, the bridge's
//! refusals and what errors may say. Gated on `SPOT_TEST_PG_URL` (skipped with a message when
//! unset, a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    PgHarness, SharedSource, bridge, checkpoint, create_schema, delete_item, fixture, pg_store,
    tenant,
};
use spot_data_policy::store::RetentionStore;
use spot_data_policy::{
    DataPolicyError, ExecutionOptions, FieldClassMap, Gregorian, Holds, PolicySet, Resume,
    RetentionRules, StatutoryFloor, execute, plan_run,
};
use spot_data_policy_pg::{PgRetentionStore, UNSUPPORTED_CONTEXT};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;

fn reason(error: &DataPolicyError) -> &'static str {
    match error {
        DataPolicyError::StoreFailed { reason } => reason,
        other => panic!("expected a store failure, got {other:?}"),
    }
}

#[test]
fn the_same_item_applied_concurrently_applies_its_effect_once() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4);
    let tenant = tenant(1);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&["a", "b"]);
    drop(pg_store(&fx, &bridge, &tenant, source.clone())); // creates the tables
    *source.delay.lock().unwrap() = Duration::from_millis(300);
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (bridge, tenant, source) = (Arc::clone(&bridge), tenant, source.clone());
        handles.push(std::thread::spawn(move || {
            let mut store = PgRetentionStore::new(bridge, common::POOL, tenant, source).unwrap();
            store.apply_item(delete_item("run", "a"))
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(source.effects(), 1, "one effect for four racing callers");
    assert_eq!(source.count(), 1);
    let store = pg_store(&fx, &bridge, &tenant, source);
    assert_eq!(store.run_outcomes("run").unwrap().len(), 1);
    assert_eq!(store.proofs().unwrap().len(), 1);
}

#[test]
fn a_failing_effect_rolls_back_and_a_retry_applies_it() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(2);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&["a"]);
    let mut store = pg_store(&fx, &bridge, &tenant, source.clone());
    source.fail.store(true, Ordering::SeqCst);
    let error = store.apply_item(delete_item("run", "a")).unwrap_err();
    assert_eq!(reason(&error), "the effect was made to fail");
    assert!(
        store.outcome("run", "a").unwrap().is_none(),
        "no outcome row"
    );
    assert!(store.proof("a").unwrap().is_none(), "no proof row");
    assert_eq!(source.count(), 1);
    source.fail.store(false, Ordering::SeqCst);
    store.apply_item(delete_item("run", "a")).unwrap();
    assert_eq!(source.effects(), 1);
    assert!(store.outcome("run", "a").unwrap().is_some());
}

#[test]
fn state_survives_a_restart_and_resume_does_each_item_once() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(3);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&["a", "b", "c"]);
    {
        let mut store = pg_store(&fx, &bridge, &tenant, source.clone());
        store.apply_item(delete_item("run", "a")).unwrap();
        store.save_checkpoint(checkpoint("run", 1)).unwrap();
    } // the store is dropped: a simulated crash between two items.
    let mut store = pg_store(&fx, &bridge, &tenant, source.clone());
    assert_eq!(store.checkpoint("run").unwrap(), Some(checkpoint("run", 1)));
    assert!(store.outcome("run", "a").unwrap().is_some());
    assert!(store.proof("a").unwrap().is_some());
    for id in ["a", "b", "c"] {
        if store.outcome("run", id).unwrap().is_none() {
            store.apply_item(delete_item("run", id)).unwrap();
        }
    }
    assert_eq!(
        source.effects(),
        3,
        "each item exactly once across the restart"
    );
}

#[test]
fn two_tenants_never_see_each_others_ledger() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let (one, two) = (tenant(4), tenant(5));
    create_schema(&fx, &one);
    create_schema(&fx, &two);
    let mut first = pg_store(&fx, &bridge, &one, SharedSource::with(&["a"]));
    let second = pg_store(&fx, &bridge, &two, SharedSource::with(&["a"]));
    first.apply_item(delete_item("run", "a")).unwrap();
    first.save_checkpoint(checkpoint("run", 1)).unwrap();
    assert!(second.outcome("run", "a").unwrap().is_none());
    assert!(second.checkpoint("run").unwrap().is_none());
    assert!(second.proof("a").unwrap().is_none());
    assert!(second.proofs().unwrap().is_empty());
    assert!(second.run_outcomes("run").unwrap().is_empty());
}

#[test]
fn prepare_is_idempotent_and_a_missing_schema_is_a_visible_failure() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(6);
    let store = PgRetentionStore::new(
        Arc::clone(&bridge),
        common::POOL,
        tenant,
        SharedSource::with(&[]),
    )
    .unwrap();
    let error = store.prepare().unwrap_err();
    assert_eq!(error.code(), "data_policy_pg.prepare_failed");
    create_schema(&fx, &tenant);
    store.prepare().unwrap();
    store.prepare().unwrap();
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(7);
    create_schema(&fx, &tenant);
    let mut store = pg_store(&fx, &bridge, &tenant, SharedSource::with(&["a"]));
    let (store_ref, error) = fx.rt.block_on(async {
        let error = store.apply_item(delete_item("run", "a")).unwrap_err();
        let read = store.ledger().outcome("run", "a").unwrap_err();
        assert!(reason(&read).starts_with("pg_bridge.wrong_thread"));
        (&mut store, error)
    });
    assert!(reason(&error).starts_with("pg_bridge.wrong_thread"));
    // Building a store inside a runtime is refused at construction.
    let built = fx.rt.block_on(async {
        PgRetentionStore::new(
            Arc::clone(&bridge),
            common::POOL,
            tenant,
            SharedSource::with(&[]),
        )
        .err()
    });
    assert_eq!(built.unwrap().code(), UNSUPPORTED_CONTEXT);
    // Nothing was written, and outside the runtime the same store works.
    assert!(store_ref.outcome("run", "a").unwrap().is_none());
    store_ref.apply_item(delete_item("run", "a")).unwrap();
}

#[test]
fn a_bridge_allowing_more_calls_than_the_pool_serves_is_refused() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4); // fine for a pool of 6, too many for a pool of 4
    let built = PgRetentionStore::new(bridge, 4, tenant(8), SharedSource::with(&[])).err();
    assert_eq!(built.unwrap().code(), UNSUPPORTED_CONTEXT);
}

/// A bridge whose database is unreachable and whose password and host are recognisable.
fn unreachable_bridge(fx: &common::Fixture) -> Arc<Bridge> {
    let mut config = fx.db.config();
    config.port = 1;
    config.host = "db-host-marker.invalid".to_owned();
    config.password = "pw-marker-s3cret".to_owned();
    config.pool_max_size = common::POOL;
    let backend = PgBackend::connect_unchecked(config).unwrap();
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "unreachable".to_owned(),
                workers: 2,
                max_in_flight: 2,
                queue_capacity: 2,
                default_deadline: Duration::from_secs(2),
                max_deadline: Duration::from_secs(5),
            },
            &backend,
        )
        .unwrap(),
    )
}

#[test]
fn an_unreachable_database_is_a_store_failure_with_no_secret_in_it() {
    let Some(fx) = fixture() else { return };
    let bridge = unreachable_bridge(&fx);
    let mut store =
        PgRetentionStore::new(bridge, common::POOL, tenant(9), SharedSource::with(&["a"])).unwrap();
    let mut texts = Vec::new();
    let error = store.apply_item(delete_item("run", "a")).unwrap_err();
    assert!(matches!(error, DataPolicyError::StoreFailed { .. }));
    texts.push(format!("{error:?} {error}"));
    // A read failure is an error from the read itself, naming the operation; never "no row".
    let reads: [(&str, DataPolicyError); 5] = [
        ("outcome", store.outcome("run", "a").unwrap_err()),
        ("run_outcomes", store.run_outcomes("run").unwrap_err()),
        ("checkpoint", store.checkpoint("run").unwrap_err()),
        ("proof", store.proof("a").unwrap_err()),
        ("proofs", store.proofs().unwrap_err()),
    ];
    for (operation, error) in reads {
        assert!(
            reason(&error).contains(operation),
            "{operation} is named in: {error}"
        );
        texts.push(format!("{error:?} {error}"));
    }
    // And a write is not blocked by a latch: it fails on its own account, and then works.
    let refused = store.save_checkpoint(checkpoint("run", 1)).unwrap_err();
    assert!(!reason(&refused).contains("earlier"));
    texts.push(format!("{refused:?} {refused}"));
    let ledger_error = store.ledger().checkpoint("run").unwrap_err();
    texts.push(format!("{ledger_error:?} {ledger_error}"));
    let prepare = store.prepare().unwrap_err();
    texts.push(format!("{prepare:?} {prepare}"));
    for text in texts {
        for secret in [
            "pw-marker-s3cret",
            "db-host-marker",
            "postgres://",
            "SELECT",
            "INSERT",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into: {text}");
        }
    }
    // The nothing-written side: the source's record is untouched.
    assert!(store.record("a").unwrap().is_some());
}

#[test]
fn a_source_read_failure_is_an_error_not_an_empty_tenant() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(5);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&["a", "b"]);
    let store = pg_store(&fx, &bridge, &tenant, source.clone());
    source.fail_reads.store(true, Ordering::SeqCst);
    assert!(matches!(
        store.records(),
        Err(DataPolicyError::StoreFailed { .. })
    ));
    assert!(matches!(
        store.record("a"),
        Err(DataPolicyError::StoreFailed { .. })
    ));
    source.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(store.records().unwrap().len(), 2);
}

/// The ledger's table disappears under a run: the read of the outcome fails, the run stops with
/// `StoreFailed` naming it, no effect is applied, and once the table is back a second run
/// completes with each effect applied once.
#[test]
fn a_ledger_read_failing_mid_run_stops_it_and_a_healthy_rerun_repeats_no_effect() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(6);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&[]);
    for id in ["a", "b", "c"] {
        let mut note = common::record(id);
        note.doctype = "note".to_owned();
        note.created = 1_577_836_800_000; // 2020-01-01
        source.inner.lock().unwrap().insert(note).unwrap();
    }
    let mut store = pg_store(&fx, &bridge, &tenant, source.clone());
    let rules = RetentionRules::from_toml(
        "[[rule]]\nid = \"notes\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n",
    )
    .unwrap();
    let (holds, floors, fields) = (
        Holds::empty(),
        StatutoryFloor::empty(),
        FieldClassMap::default(),
    );
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = 1_767_225_600_000; // 2026-01-01
    let plan = plan_run(&policy, &store, now).unwrap();
    assert_eq!(plan.items().len(), 3);
    let options = ExecutionOptions {
        dry_run: false,
        batch_size: 1,
        ..ExecutionOptions::default()
    };
    let schema = tenant.schema_name();
    let raw = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(raw.batch_execute(&format!(
            "ALTER TABLE \"{schema}\".spot_retention_outcome RENAME TO spot_retention_outcome_away"
        )))
        .unwrap();
    let error = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap_err();
    assert!(
        reason(&error).contains("outcome"),
        "the operation is named: {error}"
    );
    assert_eq!(
        source.effects(),
        0,
        "nothing was applied after the failed read"
    );
    assert_eq!(source.count(), 3);
    assert!(
        store
            .checkpoint(&format!("{}:{}", options.kind, plan.identity_hex()))
            .unwrap()
            .is_none(),
        "the checkpoint is as it was"
    );
    fx.rt
        .block_on(raw.batch_execute(&format!(
            "ALTER TABLE \"{schema}\".spot_retention_outcome_away RENAME TO spot_retention_outcome"
        )))
        .unwrap();
    let report = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();
    assert!(report.completed);
    assert_eq!(report.applied(), 3);
    assert_eq!(source.effects(), 3, "each effect applied once");
    assert_eq!(source.count(), 0);
}

#[test]
fn a_real_engine_run_deletes_records_keeps_proofs_and_resumes_cleanly() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 2);
    let tenant = tenant(10);
    create_schema(&fx, &tenant);
    let source = SharedSource::with(&[]);
    for id in ["n1", "n2", "n3"] {
        let mut note = common::record(id);
        note.doctype = "note".to_owned();
        note.created = 1_577_836_800_000; // 2020-01-01
        source.inner.lock().unwrap().insert(note).unwrap();
    }
    let mut harness = PgHarness {
        store: pg_store(&fx, &bridge, &tenant, source.clone()),
        source: source.clone(),
    };
    let rules = RetentionRules::from_toml(
        "[[rule]]\nid = \"notes\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n",
    )
    .unwrap();
    let (holds, floors, fields) = (
        Holds::empty(),
        StatutoryFloor::empty(),
        FieldClassMap::default(),
    );
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = 1_767_225_600_000; // 2026-01-01
    let plan = plan_run(&policy, &harness.store, now).unwrap();
    assert_eq!(plan.items().len(), 3);
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    let report = execute(
        &plan,
        &policy,
        &mut harness.store,
        &options,
        Resume::Fresh,
        now,
    )
    .unwrap();
    assert!(report.completed);
    assert_eq!(report.applied(), 3);
    assert_eq!(source.count(), 0);
    assert_eq!(harness.store.proofs().unwrap().len(), 3);
    for proof in harness.store.proofs().unwrap() {
        proof.verify(&harness.store).unwrap();
    }
    // A fresh store on the same database sees the finished run: a second start is refused, a
    // resume of a complete run is refused, and neither repeats an effect.
    let mut again = pg_store(&fx, &bridge, &tenant, source.clone());
    let started = execute(&plan, &policy, &mut again, &options, Resume::Fresh, now).unwrap_err();
    assert!(matches!(started, DataPolicyError::RunMismatch { .. }));
    let resumed = execute(&plan, &policy, &mut again, &options, Resume::Continue, now).unwrap_err();
    assert!(matches!(resumed, DataPolicyError::RunMismatch { .. }));
    assert_eq!(again.run_outcomes(&report.run_id).unwrap().len(), 3);
    assert_eq!(again.proofs().unwrap().len(), 3);
    assert_eq!(source.effects(), 3, "no effect was repeated");
}
