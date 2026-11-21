//! The job pipeline under kill -9 on a scratch PostgreSQL. Without PostgreSQL binaries
//! (`SPOT_PG_BIN` or `/usr/lib/postgresql/18/bin`) each test prints `SKIPPED`; under
//! `SPOT_REQUIRE_PG=1` that is a failure. `SPOT_L8_FAULTS_PROFILE=smoke|standard|nightly`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_l8_faults::cluster::pg_bin_dir;
use spot_l8_faults::{ClusterConfig, Profile, ScratchCluster, profile_from_env, skip_or_fail};
use spot_l8_job_faults::pipeline::Guard;
use spot_l8_job_faults::scenarios::{Config, Kind, run};

async fn scenario(kind: Kind, smoke: bool) {
    let Some(bin) = pg_bin_dir() else {
        let _ = skip_or_fail("a job pipeline fault scenario");
        return;
    };
    let profile = profile_from_env();
    if profile == Profile::Smoke && !smoke {
        println!("skipped in the smoke profile: {}", kind.name());
        return;
    }
    let seeds: Vec<u64> = if profile == Profile::Nightly {
        (0..5).map(|i| 20_261_113 + i).collect()
    } else {
        vec![20_261_113]
    };
    for seed in seeds {
        let cluster = ScratchCluster::start(ClusterConfig::new(bin.clone()))
            .await
            .expect("a scratch cluster");
        let outcome = run(
            kind,
            &cluster,
            &Config::for_profile(profile, seed),
            Guard::Guarded,
        )
        .await
        .unwrap();
        let report = outcome.report();
        println!("{report}");
        assert!(outcome.ok(), "{report}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_before_intake() {
    scenario(Kind::KillBeforeIntake, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_between_intake_and_claim() {
    scenario(Kind::KillBetweenIntakeAndClaim, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_while_running() {
    scenario(Kind::KillWhileRunning, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_during_effect_commit() {
    scenario(Kind::KillDuringEffectCommit, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_kills() {
    scenario(Kind::RepeatedKills, false).await;
}

/// A demonstration, not part of the green suite: a handler whose insert is not idempotent
/// produces a duplicate effect when the handler's commit is cut, and check (a) reports it. Like
/// the `fsync = off` demonstration of `l8-faults`, it needs several runs to show the failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "honesty demonstration: shows the checks can fail; run by hand"]
async fn an_unguarded_handler_duplicates_an_effect_and_check_a_says_so() {
    let Some(bin) = pg_bin_dir() else {
        return;
    };
    let mut found = false;
    for seed in 0..5 {
        let cluster = ScratchCluster::start(ClusterConfig::new(bin.clone()))
            .await
            .unwrap();
        let outcome = run(
            Kind::KillDuringEffectCommit,
            &cluster,
            &Config::for_profile(Profile::Standard, 100 + seed),
            Guard::Unguarded,
        )
        .await
        .unwrap();
        println!("{}", outcome.report());
        found |= outcome
            .violations
            .iter()
            .any(|v| v.check == 'a' && v.detail.contains("effect rows"));
    }
    assert!(
        found,
        "no duplicate effect in 5 runs: raise the cuts or the runs"
    );
}
