//! The naming workload and `naming-gapless`, on the in-memory fake.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_engine::naming::NamingConfig;
use spot_l7_engine::runner::run_naming;
use spot_l7_engine::world::World;
use spot_storage_fake::FakeBackend;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn naming_is_gapless_under_parallel_create_and_rollback() {
    let world = Arc::new(World::new());
    let config = NamingConfig {
        seed: 424_242,
        workers: 8,
        operations: 600,
        rollback_one_in: 3,
        profile: "smoke".to_owned(),
    };
    let (naming, report) = run_naming(world, config, Arc::new(FakeBackend::new()))
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert!(
        naming.rollbacks > 0,
        "the workload must roll back some allocations"
    );
    assert!(
        !naming.series.is_empty(),
        "the workload must use the series"
    );
    assert!(report.ok, "{}", report.to_text());
    for (series, numbers) in &naming.series {
        let top = numbers.iter().next_back().copied().unwrap_or(0);
        assert_eq!(
            numbers.len() as u64,
            top,
            "series {series} must be dense 1..=n"
        );
    }
}
