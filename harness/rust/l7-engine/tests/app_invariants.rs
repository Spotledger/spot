//! App invariant registration: the sample app's invariant runs next to the framework's, and an
//! invariant that panics or runs long is a failed invariant, not a crashed run.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_l7_engine::app::{GuardedInvariant, SampleProjectTaskCount, register_app_invariants};
use spot_l7_engine::corrupt::proof_config;
use spot_l7_engine::runner::{check_engine_extended, run_engine};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::{BoxFuture, CheckContext, Invariant, Violation};
use spot_storage_fake::FakeBackend;

struct Panics;

impl Invariant for Panics {
    fn id(&self) -> &str {
        "app.panics"
    }
    fn describe(&self) -> &str {
        "an app invariant that panics"
    }
    fn check<'a>(&'a self, _ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        Box::pin(async move { panic!("the app invariant panicked") })
    }
}

struct Slow;

impl Invariant for Slow {
    fn id(&self) -> &str {
        "app.slow"
    }
    fn describe(&self) -> &str {
        "an app invariant that runs long"
    }
    fn check<'a>(&'a self, _ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Vec::new()
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sample_invariant_runs_and_a_panicking_or_slow_one_fails_cleanly() {
    let world = Arc::new(World::new());
    let (run, before) = run_engine(
        world.clone(),
        proof_config(11_223_344, "fake"),
        FakeBackend::new(),
    )
    .await
    .unwrap();
    assert!(before.ok, "the healthy run passes: {}", before.to_text());

    let report = check_engine_extended(&world, &run, |registry| {
        register_app_invariants("sample", registry, |app| {
            app.register(Box::new(SampleProjectTaskCount::new(run.oracle.clone())))?;
            Ok(())
        })
        .expect("the sample invariant registers");
        registry
            .register(Box::new(GuardedInvariant::new(
                Box::new(Panics),
                Duration::from_millis(500),
            )))
            .expect("the panicking invariant registers");
        registry
            .register(Box::new(GuardedInvariant::new(
                Box::new(Slow),
                Duration::from_millis(50),
            )))
            .expect("the slow invariant registers");
    })
    .await;

    println!("{}", report.to_text());
    // The sample app's invariant holds on a healthy run.
    let sample = report
        .results
        .iter()
        .find(|r| r.id == "sample.project_task_count")
        .expect("the sample invariant is in the report");
    assert!(sample.passed, "{}", report.to_text());
    // The panicking invariant is a failed invariant, not a crash.
    assert_eq!(
        report.violations_of("app.panics")[0].detail,
        "invariant_panicked"
    );
    // The slow invariant times out and is reported, not hung on.
    assert_eq!(
        report.violations_of("app.slow")[0].detail,
        "invariant_timed_out"
    );
    assert!(report.failed_ids().contains(&"app.panics"));
    assert!(report.failed_ids().contains(&"app.slow"));
}

#[test]
fn an_app_invariant_must_be_namespaced_and_unique() {
    let mut registry = spot_l7_invariants::invariant::InvariantRegistry::new();
    let err = register_app_invariants("sample", &mut registry, |app| {
        app.register(Box::new(Panics)) // id `app.panics`, not `sample.*`
    })
    .unwrap_err();
    assert!(matches!(
        err,
        spot_l7_invariants::invariant::RegistryError::Duplicate(_)
    ));
}
