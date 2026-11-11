//! The suite's entry point: one fixture, every case, one report.
//!
//! `SPOT_TEST_PG_URL` names the PostgreSQL admin database (`SPOT_REQUIRE_PG=1` makes a missing
//! database a failure instead of a skip). `L3_QUICK=1` runs the core subset the pull-request job
//! uses; `L3_JUNIT` names the JUnit document the CI job uploads, and `L3_ARTIFACTS` the directory
//! a failing run's fixture log is copied into before teardown.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_http::Fixture;
use l3_http::cases;
use l3_http::report::{Filter, Report};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_framework_works_over_the_real_stack() {
    let Some(mut fixture) = Fixture::start().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL (and SPOT_REQUIRE_PG=1 so CI cannot skip)");
        return;
    };
    let filter = Filter::from_env();
    let report: Report = cases::run_all(&fixture, filter).await;
    if report.failed() > 0 {
        // The root (and the log in it) is removed by `finish`; keep the log the failure exists
        // for. The copy happens before teardown, and the drop guard copies it too if a case
        // panics before the report exists.
        match fixture.preserve_log() {
            Some(path) => println!("the fixture log is at {}", path.display()),
            None => println!(
                "could not copy the fixture log from {}",
                fixture.log().display()
            ),
        }
    }
    fixture.finish().await;
    report.finish("l3-http");
}
