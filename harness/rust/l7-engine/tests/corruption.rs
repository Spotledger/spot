//! Seeded-corruption proofs: a healthy run passes every invariant, then exactly the named
//! invariant fails when its promise is broken.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_engine::corrupt::{self, Proof};
use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::jobs::{JobConfig, JobRun, check_jobs, run_jobs};
use spot_l7_engine::naming::NamingConfig;
use spot_l7_engine::runner::{check_engine_with, check_naming, run_engine, run_naming};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::RunMeta;
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

const SEED: u64 = 9_090_909;

fn proof(
    corruption: &'static str,
    failed_before: Vec<String>,
    failed_after: Vec<String>,
    violations: Vec<spot_l7_invariants::invariant::Violation>,
) -> Proof {
    Proof {
        corruption,
        failed_before,
        failed_after,
        violations,
        seed: SEED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_engine_corruption_is_caught_by_exactly_its_invariant() {
    let world = Arc::new(World::new());
    let (run, before) = run_engine(
        world.clone(),
        corrupt::proof_config(SEED, "fake"),
        FakeBackend::new(),
    )
    .await
    .unwrap();
    assert!(before.ok, "the healthy run must pass: {}", before.to_text());
    let before_ids = before
        .failed_ids()
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>();

    let cases: Vec<(
        &'static str,
        Arc<spot_l7_engine::driver::EngineRun>,
        &[&str],
    )> = vec![
        (
            "state_rewound",
            Arc::new(corrupt::state_rewound(&run.oracle)),
            &["lifecycle-state_machine"],
        ),
        (
            "missing_event",
            Arc::new(corrupt::missing_event(&run.oracle)),
            &["events-per_operation"],
        ),
        (
            "orphan_event",
            Arc::new(corrupt::orphan_event(&run.oracle)),
            &["events-per_operation"],
        ),
        (
            "secret_in_event",
            Arc::new(corrupt::secret_in_event(&run.oracle)),
            &["audit-no_secret_in_events"],
        ),
        (
            "denied_create_took_effect",
            Arc::new(corrupt::denied_create_took_effect(&run.oracle)),
            &["perm-no_effect_on_denial"],
        ),
        (
            "denied_write_left_version",
            Arc::new(corrupt::denied_write_left_version(&run.oracle)),
            &["perm-no_effect_on_denial"],
        ),
        (
            "links_to_missing",
            Arc::new(corrupt::links_to_missing(&run.oracle)),
            &["links-checked"],
        ),
        (
            "duplicate_name",
            Arc::new(corrupt::duplicate_name(&run.oracle)),
            &["naming-unique"],
        ),
        (
            "illegal_transition",
            Arc::new(corrupt::illegal_transition(&run.oracle)),
            &["lifecycle-state_machine"],
        ),
    ];
    for (name, oracle, expected) in cases {
        let after = check_engine_with(&world, &run, oracle).await;
        let p = proof(
            name,
            before_ids.clone(),
            after.failed_ids().iter().map(|s| (*s).to_owned()).collect(),
            after
                .results
                .iter()
                .flat_map(|r| r.violations.clone())
                .collect(),
        );
        assert!(
            p.holds(expected),
            "corruption `{name}` was not caught as expected: {p:#?}\n{}",
            after.to_text()
        );
    }

    // The store corruption: an event published for a transaction that never committed.
    corrupt::enqueue_orphan_event(&run.engine, world.catalog.clone())
        .await
        .unwrap();
    let after = check_engine_with(&world, &run, run.oracle.clone()).await;
    assert_eq!(after.failed_ids(), vec!["events-per_operation"]);
    assert!(
        after
            .violations_of("events-per_operation")
            .iter()
            .any(|v| v.detail == "unknown_event")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_naming_gap_is_caught_by_naming_gapless() {
    let world = Arc::new(World::new());
    let backend = Arc::new(FakeBackend::new());
    let config = NamingConfig {
        seed: SEED,
        workers: 4,
        operations: 400,
        rollback_one_in: 3,
        profile: "smoke".to_owned(),
    };
    let (naming, before) = run_naming(world.clone(), config.clone(), backend.clone())
        .await
        .unwrap();
    assert!(before.ok, "{}", before.to_text());
    let meta = RunMeta {
        seed: SEED,
        backend: "naming".to_owned(),
        profile: config.profile.clone(),
        workers: config.workers,
    };
    let corrupted = Arc::new(corrupt::gap_in_series(&naming));
    let after = check_naming(&world, &corrupted, meta, backend).await;
    assert_eq!(after.failed_ids(), vec!["naming-gapless"]);
    assert!(
        after
            .violations_of("naming-gapless")
            .iter()
            .any(|v| v.detail.starts_with("gap number=") || v.detail.starts_with("next_number"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_job_corruption_is_caught_by_exactly_its_invariant() {
    let world = Arc::new(World::new());
    let run = run_jobs(JobConfig::default()).await;
    let meta = RunMeta {
        seed: JobConfig::default().seed,
        backend: "jobs".to_owned(),
        profile: "smoke".to_owned(),
        workers: 1,
    };
    let before = check_jobs(&Arc::new(run.clone()), world.catalog.clone(), meta.clone()).await;
    assert!(before.ok, "{}", before.to_text());
    let before_ids = before
        .failed_ids()
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>();

    let cases: Vec<(&'static str, JobRun, &[&str])> = vec![
        ("job_lost", corrupt::job_lost(&run), &["jobs-no_lost_job"]),
        (
            "job_double_effect",
            corrupt::job_double_effect(&run),
            &["jobs-exactly_one_effect_per_idempotency_key"],
        ),
        (
            "job_over_retry",
            corrupt::job_over_retry(&run),
            &["jobs-retry_bounds"],
        ),
    ];
    for (name, corrupted, expected) in cases {
        let after = check_jobs(&Arc::new(corrupted), world.catalog.clone(), meta.clone()).await;
        let p = proof(
            name,
            before_ids.clone(),
            after.failed_ids().iter().map(|s| (*s).to_owned()).collect(),
            after
                .results
                .iter()
                .flat_map(|r| r.violations.clone())
                .collect(),
        );
        assert!(
            p.holds(expected),
            "job corruption `{name}` was not caught as expected: {p:#?}\n{}",
            after.to_text()
        );
    }
}

#[test]
fn the_proof_helper_rejects_a_partial_catch() {
    let p = proof("x", Vec::new(), vec!["a".to_owned()], Vec::new());
    assert!(
        !p.holds(&["a"]),
        "a proof needs a violation naming the invariant"
    );
    let p = proof(
        "x",
        vec!["clean".to_owned()],
        vec!["a".to_owned()],
        Vec::new(),
    );
    assert!(
        !p.holds(&["a"]),
        "a proof needs a clean run before the corruption"
    );
}

/// A profile check the CI script relies on.
#[test]
fn the_smoke_profile_is_the_default_size() {
    let p = Profile::Smoke;
    assert_eq!(p.workers(), 8);
    assert_eq!(p.operations(), 2_000);
    let _ = DriverConfig::for_profile(p, SEED, "fake");
}
