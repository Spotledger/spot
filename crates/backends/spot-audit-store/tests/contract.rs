//! The `spot-audit-chain` contract suite against PostgreSQL, and concurrent append.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditRecord, CheckpointPolicy, KeyRing, LocalSigner, Verifier, contract,
};

fn record(n: i64) -> AuditRecord {
    AuditRecord {
        kind: "doc.update".to_owned(),
        actor: "user:u1".to_owned(),
        on_behalf_of: None,
        subject: None,
        at: n,
        payload: Default::default(),
        masked: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_passes_the_audit_store_contract() {
    let Some(db) = common::db().await else { return };
    let store = common::store(&db).await;
    contract::run(&store, "wpaidb-contract")
        .await
        .expect("the PostgreSQL store passes the contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_never_fork_the_chain() {
    let Some(db) = db_guard().await else { return };
    let signer: Arc<dyn spot_audit_chain::CheckpointSigner> =
        Arc::new(LocalSigner::from_seed([3; 32]));

    // Two independent writers, each with its own chain over the same database and site.
    let mut tasks = Vec::new();
    for writer in 0..2 {
        let chain = AuditChain::new(
            common::store(&db).await,
            "wpaidb-race",
            Arc::clone(&signer),
            CheckpointPolicy { every: 0 },
        );
        tasks.push(tokio::spawn(async move {
            for n in 0..25 {
                chain.append(record(i64::from(writer) * 100 + n)).await?;
            }
            Ok::<_, spot_audit_chain::AuditError>(())
        }));
    }
    for task in tasks {
        task.await.expect("a writer task").expect("appends");
    }

    // The chain is 1..=50 with no fork: the verifier sees no gap, link break or duplicate.
    let store = common::store(&db).await;
    let mut keys = KeyRing::new();
    keys.add(&LocalSigner::from_seed([3; 32]).public_key())
        .expect("key");
    let report = Verifier::new(&keys)
        .verify_range(&store, "wpaidb-race", 1, u64::MAX)
        .await
        .expect("verify");
    assert!(report.is_ok(), "{:?}", report.failures);
    assert_eq!(report.checked, 50);
}

async fn db_guard() -> Option<spot_postgres::testing::TestDb> {
    common::db().await
}
