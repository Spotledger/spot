//! The fault scenarios. The smoke pair runs in `cargo test` (skipped without PostgreSQL); every
//! standard scenario is `#[ignore]`d and run by `ci/run.sh`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_l8_faults::cluster::{ClusterConfig, ScratchCluster, pg_bin_dir};
use spot_l8_faults::profile::Profile;
use spot_l8_faults::profile::skip_or_fail;
use spot_l8_faults::scenarios::{
    ScenarioConfig, ScenarioResult, commit_ack_loss, crash_during_load, job_engine,
    killed_connections, partition, repeated_crashes, slow_storage,
};

async fn cluster() -> Option<ScratchCluster> {
    let bin = pg_bin_dir()?;
    Some(
        ScratchCluster::start(ClusterConfig::new(bin))
            .await
            .expect("the cluster starts"),
    )
}

fn report(name: &str, result: &ScenarioResult) {
    println!("{}", result.to_report().to_text());
    write_artifacts(name, result);
    assert!(result.l7_ok, "{name}: {}", result.l7_text);
    assert!(result.audit.ok, "{name}: {}", result.audit.table());
    assert!(
        result.isolation_ok.unwrap_or(true),
        "{name}: {}",
        result.isolation_text.clone().unwrap_or_default()
    );
}

/// Writes the JSON report and the fault timeline for a scenario, when the CI script asked for it.
fn write_artifacts(name: &str, result: &ScenarioResult) {
    let Ok(dir) = std::env::var("SPOT_L8_FAULTS_OUT") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/{name}.json"), result.to_report().to_json());
    let mut timeline = String::new();
    for event in &result.timeline {
        timeline.push_str(&format!(
            "{:>10}us conn={} dir={} fault={} {}\n",
            event.at_us,
            event.conn,
            event.direction.name(),
            event.fault.name(),
            event.detail
        ));
    }
    let _ = std::fs::write(format!("{dir}/{name}.timeline.txt"), timeline);
}

// -- smoke (in `cargo test`) -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_during_load_smoke() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("crash_during_load");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Smoke, 7);
    let result = crash_during_load(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("crash_during_load", &result);
    assert!(
        !result.timeline.is_empty() || result.recovery_ms.is_some(),
        "the crash must have been recovered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_ack_loss_smoke() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("commit_ack_loss");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Smoke, 11);
    let result = commit_ack_loss(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("commit_ack_loss", &result);
    assert!(
        !result.timeline.is_empty(),
        "the commit-ack-loss fault must have fired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_engine_smoke() {
    let cfg = ScenarioConfig::for_profile(Profile::Smoke, 5);
    let result = job_engine(&cfg).await;
    report("job_engine", &result);
}

// -- standard (ignored; run by ci/run.sh) ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn crash_during_load_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("crash_during_load");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 23);
    let result = crash_during_load(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("crash_during_load", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn repeated_crashes_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("repeated_crashes");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 29);
    let result = repeated_crashes(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("repeated_crashes", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn killed_connections_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("killed_connections");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 31);
    let result = killed_connections(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("killed_connections", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn commit_ack_loss_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("commit_ack_loss");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 37);
    let result = commit_ack_loss(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("commit_ack_loss", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn partition_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("partition");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 41);
    let result = partition(&cluster, &cfg).await.expect("the scenario runs");
    report("partition", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn slow_storage_standard() {
    let Some(cluster) = cluster().await else {
        skip_or_fail("slow_storage");
        return;
    };
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 43);
    let result = slow_storage(&cluster, &cfg)
        .await
        .expect("the scenario runs");
    report("slow_storage", &result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "standard profile"]
async fn job_engine_standard() {
    let cfg = ScenarioConfig::for_profile(Profile::Standard, 47);
    let result = job_engine(&cfg).await;
    report("job_engine", &result);
}
