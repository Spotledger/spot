//! `spot migrate-plan` and `spot migrate-approve`: argument handling, and plan, approve and show
//! over the real PostgreSQL stores (`SPOT_TEST_PG_URL`; skipped with a printed reason when unset).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

use std::sync::Arc;

use spot_core::{Clock, ManualClock};
use spot_migration_approval::ApprovalStore;
use spot_migration_ops::approve::{approve, reject};
use spot_migration_ops::plan::{PlanInputs, plan, show};
use spot_migration_ops::{LifecycleError, PlanState};
use spot_migration_plan::{
    SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind, SystemDescription,
};
use spot_permissions::Principal;
use spot_pg_migrate::{ApplyOptions, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};
use time::macros::datetime;

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::migrate_plan::{build_lifecycle, on_plain_thread, principal_of};

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(datetime!(2026-11-12 09:00:00 UTC)))
}

fn additive_inputs() -> PlanInputs {
    PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        SchemaPlan {
            steps: vec![SchemaStep {
                key: "add:party.nickname".to_owned(),
                kind: SchemaStepKind::AddColumn,
                entity: "party".to_owned(),
                object: Some("nickname".to_owned()),
                needs_backfill: false,
                run: SchemaRunKind::Transaction,
                sentence: "add the nickname column".to_owned(),
            }],
            refusals: Vec::new(),
        },
    )
}

#[test]
fn the_commands_parse_their_arguments() {
    let plan = commands::migrate_plan::command()
        .try_get_matches_from(["migrate-plan", "plan", "--site", "alpha", "--json"])
        .unwrap();
    let (name, sub) = plan.subcommand().unwrap();
    assert_eq!(name, "plan");
    assert!(sub.get_flag("json"));
    assert!(
        commands::migrate_plan::command()
            .try_get_matches_from(["migrate-plan", "plan"])
            .is_err(),
        "--site is required"
    );
    assert!(
        commands::migrate_approve::command()
            .try_get_matches_from(["migrate-approve", "approve", "abc", "--site", "alpha"])
            .is_err(),
        "--as is required to approve"
    );
    let approve = commands::migrate_approve::command()
        .try_get_matches_from([
            "migrate-approve",
            "approve",
            "abc",
            "--site",
            "alpha",
            "--as",
            "bob",
            "--comment",
            "ok",
        ])
        .unwrap();
    let (_, sub) = approve.subcommand().unwrap();
    assert_eq!(sub.get_one::<String>("hash").unwrap(), "abc");
}

#[test]
fn the_identity_is_the_system_unless_a_person_is_named() {
    let matches = commands::migrate_plan::command()
        .try_get_matches_from(["migrate-plan", "plan", "--site", "alpha"])
        .unwrap();
    let (_, sub) = matches.subcommand().unwrap();
    assert!(principal_of(sub).is_system);
    let matches = commands::migrate_plan::command()
        .try_get_matches_from(["migrate-plan", "plan", "--site", "alpha", "--as", "bob"])
        .unwrap();
    let (_, sub) = matches.subcommand().unwrap();
    let who = principal_of(sub);
    assert_eq!(who.user, "bob");
    assert!(!who.is_system);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_approve_and_show_over_postgres_with_separation_of_duties() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let mut config = db.config();
    config.pool_max_size = 8;
    let backend = PgBackend::connect_unchecked(config).unwrap();
    // The tables come from the migrator, as `spot migrate` creates them.
    let catalog = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let options = PlanOptions {
        extra_tables: spot_migration_ops_pg::schema::extra_tables().unwrap(),
        ..PlanOptions::default()
    };
    let migration = dry_run(&backend, &catalog, &options).await.unwrap();
    apply(&backend, &migration, ApplyOptions::default())
        .await
        .unwrap();

    let open = |name: &str| {
        build_lifecycle(
            &backend,
            "alpha",
            clock(),
            Principal::with_roles(name, &["release_manager"]),
        )
        .unwrap()
    };
    let alice = open("alice");
    let bob = open("bob");
    let carol = open("carol");

    // Alice plans. The strictest policy (no policy is set) needs two approvers even for an
    // additive plan.
    let inputs = additive_inputs();
    let planned = on_plain_thread(move || {
        alice.store.ensure_present().unwrap();
        let report = plan(&alice.lifecycle, &inputs).unwrap();
        let again = plan(&alice.lifecycle, &additive_inputs()).unwrap();
        let own = approve(&alice.lifecycle, report.hash.as_str(), "mine");
        (report, again, own)
    })
    .unwrap();
    let (report, again, own) = planned;
    assert_eq!(report.state, PlanState::Proposed);
    assert!(report.created);
    assert!(
        !again.created,
        "the same inputs give the same plan and no second row"
    );
    assert_eq!(own.unwrap_err(), LifecycleError::SelfApproval);

    let hash = report.hash.as_str().to_owned();
    let (h1, h2) = (hash.clone(), hash.clone());
    let first = on_plain_thread(move || approve(&bob.lifecycle, &h1, "first").unwrap()).unwrap();
    assert_eq!(first.state, PlanState::Proposed);
    let second =
        on_plain_thread(move || approve(&carol.lifecycle, &h2, "second").unwrap()).unwrap();
    assert_eq!(second.state, PlanState::Approved);
    assert!(second.approvals.ready);

    // A new process sees the same state: it is in the database.
    let dave = open("dave");
    let shown = on_plain_thread(move || show(&dave.lifecycle, &hash, None).unwrap()).unwrap();
    assert_eq!(shown.state, PlanState::Approved);
    assert_eq!(shown.author, "alice");
    assert_eq!(shown.approvals.approved_by.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unprivileged_user_is_refused_with_exit_two() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let mut config = db.config();
    config.pool_max_size = 8;
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let eve = build_lifecycle(
        &backend,
        "alpha",
        clock(),
        Principal::with_roles("eve", &["Clerk"]),
    )
    .unwrap();
    let refused =
        on_plain_thread(move || plan(&eve.lifecycle, &additive_inputs()).unwrap_err()).unwrap();
    assert_eq!(refused.error, LifecycleError::NotPermitted);
    assert_eq!(commands::migrate_plan::exit_of(&refused.error).code(), 2);
}
