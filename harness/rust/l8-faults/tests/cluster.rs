//! The scratch cluster's lifecycle, and the `#[ignore]`d demonstration that the audit can fail.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_l7_invariants::history::{History, HistoryEntry, Level, OpKind, Outcome, Role, Touched};
use spot_l7_invariants::invariant::CheckContext;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::oracle::{DocWrite, Effects, Oracle};
use spot_l8_faults::audit::DurabilityAudit;
use spot_l8_faults::cluster::{ClusterConfig, ScratchCluster, pg_bin_dir};
use spot_l8_faults::profile::skip_or_fail;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    ChangeOp, DocumentData, DocumentStore, Fields, Isolation, Name, RecordId, StorageBackend,
    Transaction, TxOptions, Value,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scratch_cluster_starts_kills_and_recovers() {
    let Some(bin) = pg_bin_dir() else {
        skip_or_fail("the cluster lifecycle");
        return;
    };
    let cluster = ScratchCluster::start(ClusterConfig::new(bin))
        .await
        .expect("the cluster starts");
    assert!(cluster.is_up(), "the cluster answers SELECT 1");
    let pid_before = cluster.pid().expect("a postmaster pid");
    cluster.kill9().expect("kill9");
    let recovery = cluster.restart_after_kill().await.expect("recovery");
    assert!(
        cluster.is_up(),
        "the cluster answers SELECT 1 after recovery"
    );
    let pid_after = cluster.pid().expect("a postmaster pid after recovery");
    assert_ne!(pid_before, pid_after, "the postmaster was replaced");
    assert!(
        cluster.recovery_errors().is_empty(),
        "clean crash recovery: {:?}",
        cluster.recovery_errors()
    );
    println!("crash recovery took {} ms", recovery.as_millis());
}

/// **Not part of the green suite.** With `fsync = off` and `synchronous_commit = off` a commit is
/// acknowledged before its WAL record is durable, so a `SIGKILL` right after the loop can lose
/// acknowledged commits. This test does not require a loss (it is probabilistic); it proves that
/// when one happens, the durability audit reports it. Run it explicitly:
///
/// ```sh
/// SPOT_PG_BIN=/usr/lib/postgresql/18/bin cargo test -p spot-l8-faults -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "demonstrates the audit can fail; fsync=off loses acknowledged commits"]
async fn fsync_off_can_lose_acknowledged_commits() {
    let Some(bin) = pg_bin_dir() else {
        skip_or_fail("the fsync-off demonstration");
        return;
    };
    let mut config = ClusterConfig::new(bin);
    config.unsafe_durability = true;
    let cluster = ScratchCluster::start(config)
        .await
        .expect("the cluster starts");

    let backend = Arc::new(
        PgBackend::connect(cluster.backend_config())
            .await
            .expect("the backend connects"),
    );
    let model = Arc::new(Model::new().unwrap());
    backend.prepare(&model.catalog).await.unwrap();

    let mut history = History::new(1, 1, "fsync-off");
    let mut effects = Effects::new(1);
    for i in 0..200u32 {
        let options = TxOptions::new(Name::new("demo").unwrap(), Isolation::ReadCommitted);
        let mut tx = backend.begin(model.catalog.clone(), options).await.unwrap();
        let id = RecordId::new_v7();
        let mut fields = Fields::new();
        fields.insert(model.name.clone(), Value::Text(format!("party-{i}")));
        fields.insert(model.marker.clone(), Value::Text(format!("m-{i}")));
        let data = DocumentData {
            entity: model.party.clone(),
            id,
            fields,
            children: BTreeMap::new(),
        };
        let version = tx.insert(&data).await.unwrap();
        tx.commit().await.unwrap(); // acknowledged with synchronous_commit = off
        history.entries.push(HistoryEntry {
            worker: 0,
            seq: u64::from(i),
            start_us: 0,
            end_us: 0,
            kind: OpKind::CreateParty,
            isolation: Level::ReadCommitted,
            outcome: Outcome::Committed,
            attempts: 1,
            touched: vec![Touched {
                entity: model.party.to_string(),
                id: id.to_string(),
                role: Role::Created,
                version: Some(version.0),
            }],
            values: BTreeMap::new(),
        });
        effects.docs.push(DocWrite {
            entity: model.party.clone(),
            id,
            op: ChangeOp::Insert,
            version: version.0,
        });
    }
    // Kill at once: the last commits' WAL may not have been written yet.
    cluster.kill9().expect("kill9");
    cluster.restart_after_kill().await.expect("recovery");

    let mut oracle = Oracle::new();
    oracle.apply(&effects);
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        Arc::new(history),
        Arc::new(oracle),
    );
    let audit = DurabilityAudit::run(&ctx).await;
    if audit.ok {
        println!("no acknowledged commit was lost this run (fsync=off is probabilistic)");
    } else {
        println!("acknowledged commits were lost, and the audit caught it:");
        println!("{}", audit.table());
        assert!(
            audit
                .violations
                .iter()
                .any(|v| v.detail.starts_with("acknowledged_effect_absent")),
            "a loss must be reported as acknowledged_effect_absent: {:?}",
            audit.violations
        );
    }
}
