//! The scenarios on a scratch PostgreSQL that the harness kills. Without PostgreSQL binaries
//! (`SPOT_PG_BIN` or `/usr/lib/postgresql/18/bin`) each test prints `SKIPPED`; under
//! `SPOT_REQUIRE_PG=1` that is a failure. `SPOT_L8_FAULTS_PROFILE=smoke|standard|nightly`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_l8_engine_faults::scenarios::{Config, Kind, SMOKE, pg_available, run, start_cluster};
use spot_l8_faults::{Profile, profile_from_env};

async fn scenario(kind: Kind) {
    if !pg_available("an engine fault scenario") {
        return;
    }
    let profile = profile_from_env();
    if profile == Profile::Smoke && !SMOKE.contains(&kind) {
        println!("skipped in the smoke profile: {}", kind.name());
        return;
    }
    let seeds: Vec<u64> = if profile == Profile::Nightly {
        (0..5).map(|i| 20_261_113 + i).collect()
    } else {
        vec![20_261_113]
    };
    for seed in seeds {
        let cluster = start_cluster().await.expect("a scratch cluster");
        let outcome = run(kind, &cluster, &Config::for_profile(profile, seed))
            .await
            .unwrap();
        let report = outcome.report();
        println!("{report}");
        assert!(outcome.ok(), "{report}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_during_engine_load() {
    scenario(Kind::CrashDuringEngineLoad).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_reply_lost() {
    scenario(Kind::CommitReplyLost).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_between_cancel_and_amend() {
    scenario(Kind::CrashBetweenCancelAndAmend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_with_committed_steps() {
    scenario(Kind::CrashWithCommittedSteps).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_crashes() {
    scenario(Kind::RepeatedCrashes).await;
}
