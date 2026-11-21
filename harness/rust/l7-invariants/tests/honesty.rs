//! The driver's honesty: weakened backends are caught during a normal run, and a number burned
//! behind the driver's back is caught on the fake as well as on PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_invariants::corrupt::burn_sequence_number;
use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::faulty::{FaultPlan, Faulty};
use spot_l7_invariants::framework_registry;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::pg::pg_env;
use spot_l7_invariants::runner::{check, run_and_check, run_workload};
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

const SEED: u64 = 31_337;

fn normal_run() -> DriverConfig {
    DriverConfig::for_profile(Profile::Smoke, SEED)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_that_drops_one_in_500_outbox_events_is_caught() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(Faulty::new(
        Arc::new(FakeBackend::new()),
        FaultPlan {
            drop_outbox_one_in: 500,
            ..FaultPlan::default()
        },
    ));
    let (_, report) = run_and_check(backend, model, normal_run(), "fake+faulty", |_| {})
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert_eq!(report.failed_ids(), vec!["outbox-complete"]);
    let missing = report
        .violations_of("outbox-complete")
        .iter()
        .filter(|v| v.detail == "missing_event")
        .count();
    // About 1,900 write transactions with a drop one in 500 is a Poisson count with mean near 4,
    // so a floor of 3 failed about a quarter of the runs. The proof is that the loss is caught.
    assert!(
        missing >= 1,
        "about 1,900 write transactions, one in 500 dropped: {missing}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_that_keeps_numbers_of_rolled_back_allocations_is_caught() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(Faulty::new(
        Arc::new(FakeBackend::new()),
        FaultPlan {
            burn_numbers_on_rollback: true,
            ..FaultPlan::default()
        },
    ));
    let (_, report) = run_and_check(backend, model, normal_run(), "fake+faulty", |_| {})
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert_eq!(report.failed_ids(), vec!["seq-gapless"]);
    assert!(
        report
            .violations_of("seq-gapless")
            .iter()
            .any(|v| v.detail.starts_with("gap number="))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_number_burned_behind_the_drivers_back_is_caught_on_the_fake() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(FakeBackend::new());
    let done = run_workload(backend.clone(), model.clone(), normal_run(), "fake")
        .await
        .unwrap();
    let registry = framework_registry(&model);
    assert!(check(backend.clone(), &model, &done, &registry).await.ok);
    burn_sequence_number(backend.as_ref(), &model, "l7-series-1")
        .await
        .unwrap();
    let report = check(backend, &model, &done, &registry).await;
    assert_eq!(report.failed_ids(), vec!["seq-gapless"]);
    let v = &report.violations_of("seq-gapless")[0];
    assert_eq!(
        (v.entity.as_str(), v.id.as_str()),
        ("sequence", "l7-series-1")
    );
    assert!(
        v.detail.starts_with("next_number expected="),
        "{}",
        v.detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_weakened_backends_are_caught_on_postgres_too() {
    let Some(env) = pg_env(Profile::Smoke.workers()).await else {
        return;
    };
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(Faulty::new(
        env.backend.clone(),
        FaultPlan {
            drop_outbox_one_in: 500,
            burn_numbers_on_rollback: true,
            ..FaultPlan::default()
        },
    ));
    let (_, report) = run_and_check(backend, model, normal_run(), "pg+faulty", |_| {})
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert_eq!(report.failed_ids(), vec!["seq-gapless", "outbox-complete"]);
}
