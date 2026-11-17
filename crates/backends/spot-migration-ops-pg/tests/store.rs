//! The stores against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a printed reason
//! when it is unset, never faked).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_migration_approval::{Approval, ApprovalError, ApprovalStore, Decision, PlanDigest};
use spot_migration_ops::{LifecycleError, PlanState, PlanStore};
use spot_migration_ops_pg::{
    MAX_PLAN_BYTES, PgMigrationStore, RecordingAlert, RehearsalLedger, RehearsalRecord,
};

use common::{Fixture, fixture, hash_of, node, stored};

fn fx_or_skip() -> Option<Fixture> {
    let fx = fixture();
    if fx.is_none() {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
    }
    fx
}

fn digest(hash: &spot_migration_ops::PlanHash) -> PlanDigest {
    serde_json::from_value(serde_json::Value::String(hash.as_str().to_owned())).unwrap()
}

#[test]
fn the_extra_tables_pass_the_migrators_rules() {
    let tables = spot_migration_ops_pg::schema::extra_tables().unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.table.as_str()).collect();
    assert_eq!(
        names,
        [
            "spot_migration_plan",
            "spot_migration_plan_author",
            "spot_migration_approval",
            "spot_migration_rehearsal"
        ]
    );
}

#[test]
fn plan_round_trip_and_listing() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "round-trip");
    let plan = stored("{\"changes\":[]}", "user:alice", 10);
    n.store.save(plan.clone()).unwrap();
    n.store.save(plan.clone()).unwrap();
    assert_eq!(n.store.load(&plan.hash).unwrap().unwrap(), plan);
    assert!(n.store.load(&hash_of("{}")).unwrap().is_none());
    n.store
        .save(stored("{\"changes\":[1]}", "user:bob", 20))
        .unwrap();
    let list = n.store.list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].hash, plan.hash);
}

#[test]
fn a_hash_that_does_not_match_its_text_is_refused() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "mismatch");
    let mut plan = stored("{}", "user:alice", 1);
    plan.json = "{\"x\":1}".to_owned();
    assert_eq!(n.store.save(plan), Err(LifecycleError::PlanChanged));
}

#[test]
fn the_size_cap_is_enforced() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "cap");
    let big = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_PLAN_BYTES));
    assert_eq!(
        n.store.save(stored(&big, "user:alice", 1)),
        Err(LifecycleError::Store)
    );
}

#[test]
fn concurrent_state_change_loses_cleanly() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "race");
    let plan = stored("{\"race\":true}", "user:alice", 1);
    n.store.save(plan.clone()).unwrap();
    let store = Arc::new(n.store.clone());
    let handles: Vec<_> = [PlanState::Approved, PlanState::Rejected]
        .into_iter()
        .map(|next| {
            let store = Arc::clone(&store);
            let hash = plan.hash.clone();
            std::thread::spawn(move || store.mark_state(&hash, PlanState::Proposed, 1, next))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(results.contains(&Err(LifecycleError::PlanChanged)));
    let loaded = n.store.load(&plan.hash).unwrap().unwrap();
    assert_eq!(loaded.version, 2);
    assert_eq!(
        n.store.mark_state(
            &hash_of("{\"nope\":1}"),
            PlanState::Proposed,
            1,
            PlanState::Approved
        ),
        Err(LifecycleError::UnknownPlan)
    );
}

#[test]
fn a_tampered_plan_is_detected_on_load_and_alerts() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "tamper");
    let plan = stored("{\"safe\":true}", "user:alice", 1);
    n.store.save(plan.clone()).unwrap();
    let client = fx.rt.block_on(fx.db.raw_client());
    // The guard refuses the rewrite even for the owner ...
    let refused = fx.rt.block_on(client.execute(
        "UPDATE spot_migration_plan SET plan_json = '{\"safe\":false}'",
        &[],
    ));
    assert!(refused.is_err());
    // ... so the test switches it off to play a superuser who edits the table underneath.
    fx.rt.block_on(async {
        client
            .batch_execute(
                "ALTER TABLE spot_migration_plan DISABLE TRIGGER spot_migration_plan_no_rewrite",
            )
            .await
            .unwrap();
        client
            .execute(
                "UPDATE spot_migration_plan SET plan_json = '{\"safe\":false}'",
                &[],
            )
            .await
            .unwrap();
    });
    assert_eq!(n.store.load(&plan.hash), Err(LifecycleError::PlanChanged));
    assert_eq!(n.alert.hashes(), vec![plan.hash.as_str().to_owned()]);
}

#[test]
fn approvals_are_append_only_and_the_role_has_no_update_or_delete() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "approvals");
    let hash = hash_of("{\"approve\":1}");
    let d = digest(&hash);
    n.store.register_plan(&d, "user:alice").unwrap();
    for (who, decision, at) in [
        ("user:bob", Decision::Approve, 5),
        ("user:carol", Decision::Reject, 6),
    ] {
        n.store
            .append(Approval {
                plan_digest: d.clone(),
                approver: who.to_owned(),
                role: "tenant_administrator".to_owned(),
                decision,
                comment: "ok".to_owned(),
                at,
                signature: None,
            })
            .unwrap();
    }
    let rows = n.store.for_digest(&d).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].approver, "user:bob");
    assert_eq!(rows[1].decision, Decision::Reject);
    assert_eq!(n.store.all().unwrap().len(), 2);

    let client = fx.rt.block_on(fx.db.raw_client());
    // No update or delete through SQL for anybody (the guard), and no grant for the role.
    assert!(
        fx.rt
            .block_on(client.execute("UPDATE spot_migration_approval SET comment = 'x'", &[]))
            .is_err()
    );
    assert!(
        fx.rt
            .block_on(client.execute("DELETE FROM spot_migration_approval", &[]))
            .is_err()
    );
    let grants = |table: &'static str, privilege: &'static str| -> bool {
        fx.rt
            .block_on(client.query_one(
                "SELECT has_table_privilege('spot_migration_app', $1, $2)",
                &[&table, &privilege],
            ))
            .unwrap()
            .get(0)
    };
    for table in [
        "spot_migration_approval",
        "spot_migration_plan_author",
        "spot_migration_rehearsal",
    ] {
        assert!(
            grants(table, "SELECT") && grants(table, "INSERT"),
            "{table}"
        );
        assert!(
            !grants(table, "UPDATE") && !grants(table, "DELETE"),
            "{table}"
        );
    }
    assert!(!grants("spot_migration_plan", "DELETE"));
    assert!(!grants("spot_migration_plan", "TRUNCATE"));
    let column = |name: &'static str| -> bool {
        fx.rt
            .block_on(client.query_one(
                "SELECT has_column_privilege('spot_migration_app', 'spot_migration_plan', $1, 'UPDATE')",
                &[&name],
            ))
            .unwrap()
            .get(0)
    };
    assert!(column("state") && column("version"));
    assert!(!column("plan_json") && !column("author") && !column("plan_hash"));
}

#[test]
fn the_author_is_recorded_once() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "author");
    let d = digest(&hash_of("{\"author\":1}"));
    assert_eq!(n.store.author_of(&d).unwrap(), None);
    n.store.register_plan(&d, "user:alice").unwrap();
    n.store.register_plan(&d, "user:alice").unwrap();
    assert!(matches!(
        n.store.register_plan(&d, "user:mallory"),
        Err(ApprovalError::AuthorConflict)
    ));
    assert_eq!(
        n.store.author_of(&d).unwrap().as_deref(),
        Some("user:alice")
    );
}

#[test]
fn rehearsals_are_kept_by_plan_and_clock() {
    let Some(fx) = fx_or_skip() else { return };
    let n = node(&fx, "rehearsal");
    let hash = hash_of("{\"rehearse\":1}");
    assert!(n.store.latest(&hash).unwrap().is_none());
    for (at, verdict) in [(10, "fail"), (20, "pass")] {
        n.store
            .record(RehearsalRecord {
                plan_hash: hash.clone(),
                at,
                verdict: verdict.to_owned(),
                report_json: "{}".to_owned(),
            })
            .unwrap();
    }
    n.store
        .record(RehearsalRecord {
            plan_hash: hash.clone(),
            at: 20,
            verdict: "pass".to_owned(),
            report_json: "{}".to_owned(),
        })
        .unwrap();
    assert_eq!(n.store.latest(&hash).unwrap().unwrap().verdict, "pass");
    assert_eq!(n.store.history(&hash).unwrap().len(), 2);
}

#[test]
fn tables_exist_after_migrate_and_a_second_run_plans_nothing() {
    let Some(fx) = fx_or_skip() else { return };
    let backend = common::backend(&fx, 8);
    let first = common::migrate(&fx, &backend);
    assert!(
        first
            .steps
            .iter()
            .any(|s| s.key == "extra_table:spot_migration_plan"),
        "{:?}",
        first.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
    );
    let second = common::plan_of(&fx, &backend);
    assert!(
        second
            .steps
            .iter()
            .all(|s| !s.key.starts_with("extra_table:")),
        "{:?}",
        second.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
    );
    let store = PgMigrationStore::new(
        common::bridge_of(&backend, "idem"),
        Arc::new(RecordingAlert::new()),
    );
    store.ensure_present().unwrap();
    store.protect().unwrap();
    store.protect().unwrap();
}

#[test]
fn a_missing_table_is_reported() {
    let Some(fx) = fx_or_skip() else { return };
    let backend = common::backend(&fx, 8);
    let store = PgMigrationStore::new(
        common::bridge_of(&backend, "missing"),
        Arc::new(RecordingAlert::new()),
    );
    assert_eq!(store.ensure_present(), Err(LifecycleError::Store));
}
