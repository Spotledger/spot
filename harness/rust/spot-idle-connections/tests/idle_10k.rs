//! The 10,000-connection run (WP-PLT-01 acceptance: "10k idle connections per node").
//!
//! `#[ignore]`d: it needs about 20,600 file descriptors and ninety seconds. The nightly job runs
//! `cargo test -p spot-idle-connections --test idle_10k -- --ignored --nocapture`; the JSON report
//! goes to stdout. `SPOT_IDLE_CONNECTIONS`, `SPOT_IDLE_HOLD_SECONDS`, `SPOT_IDLE_BUSY_CLIENTS` and
//! `SPOT_IDLE_BUSY_RPS` override the defaults. With `SPOT_IDLE_REQUIRE=1` (the nightly job) a
//! skipped run (for example a descriptor limit that is too low) fails the test instead of passing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use spot_idle_connections::{Budgets, Outcome, RunConfig, run};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ~20,600 file descriptors and ~90 s; the nightly job passes --ignored"]
async fn ten_thousand_idle_connections_stay_within_the_budgets() {
    let config = RunConfig::from_env();
    let outcome = run(&config, &Budgets::shipped().unwrap()).await.unwrap();
    println!("{}", outcome.to_json());
    match outcome {
        Outcome::Skipped(reason) => {
            assert!(
                std::env::var("SPOT_IDLE_REQUIRE").as_deref() != Ok("1"),
                "the run was skipped but SPOT_IDLE_REQUIRE=1: {reason}"
            );
            println!("SKIPPED: {reason}");
        }
        Outcome::Completed(report) => {
            let failed: Vec<_> = report
                .failed()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect();
            assert!(failed.is_empty(), "budgets missed: {failed:?}");
        }
    }
}
