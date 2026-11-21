//! Security: the role that cannot rewrite history, the guard trigger, a one-byte tamper caught
//! at the exact `seq`, and a copied chain that verifies in a fresh database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditRecord, CheckpointPolicy, CheckpointSigner, KeyRing, LocalSigner,
};
use spot_audit_store::VerifyJob;

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

fn signer() -> (Arc<LocalSigner>, Arc<dyn CheckpointSigner>) {
    let local = Arc::new(LocalSigner::from_seed([9; 32]));
    let dynamic: Arc<dyn CheckpointSigner> = local.clone();
    (local, dynamic)
}

fn keyring(local: &LocalSigner) -> KeyRing {
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).expect("the key adds");
    keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_application_role_cannot_rewrite_history() {
    let Some(db) = common::db().await else { return };
    let (_, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&db).await,
        "sec",
        dynamic,
        CheckpointPolicy { every: 0 },
    );
    chain.append(record(1)).await.expect("append");

    let client = common::raw(&db).await;
    let row = client
        .query_one(
            "SELECT has_table_privilege('spot_audit_app', 'spot_audit_chain', 'UPDATE'), \
                    has_table_privilege('spot_audit_app', 'spot_audit_chain', 'DELETE'), \
                    has_table_privilege('spot_audit_app', 'spot_audit_chain', 'TRUNCATE'), \
                    has_table_privilege('spot_audit_app', 'spot_audit_chain', 'INSERT'), \
                    has_table_privilege('spot_audit_app', 'spot_audit_chain', 'SELECT')",
            &[],
        )
        .await
        .expect("privilege check");
    assert!(!row.get::<_, bool>(0), "the role has UPDATE");
    assert!(!row.get::<_, bool>(1), "the role has DELETE");
    assert!(!row.get::<_, bool>(2), "the role has TRUNCATE");
    assert!(row.get::<_, bool>(3), "the role cannot append");
    assert!(row.get::<_, bool>(4), "the role cannot read");

    // As the role itself, the rewrites are refused with `permission denied`.
    client
        .batch_execute("SET ROLE spot_audit_app")
        .await
        .expect("set role");
    for sql in [
        "UPDATE spot_audit_chain SET at = at WHERE site = 'sec'",
        "DELETE FROM spot_audit_chain WHERE site = 'sec'",
    ] {
        let error = client.execute(sql, &[]).await.expect_err("refused");
        assert_eq!(error.code().map(|c| c.code()), Some("42501"), "{sql}");
    }
    let error = client
        .batch_execute("TRUNCATE spot_audit_chain")
        .await
        .expect_err("refused");
    assert_eq!(error.code().map(|c| c.code()), Some("42501"));
    client
        .batch_execute("RESET ROLE")
        .await
        .expect("reset role");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guard_trigger_refuses_the_owner_too() {
    let Some(db) = common::db().await else { return };
    let (_, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&db).await,
        "guard",
        dynamic,
        CheckpointPolicy { every: 0 },
    );
    chain.append(record(1)).await.expect("append");

    let client = common::raw(&db).await;
    let error = client
        .execute(
            "UPDATE spot_audit_chain SET at = at WHERE site = 'guard'",
            &[],
        )
        .await
        .expect_err("refused");
    assert_eq!(
        error.code().map(|c| c.code()),
        Some("42501"),
        "the trigger lets the owner through"
    );
    let error = client
        .execute("DELETE FROM spot_audit_chain WHERE site = 'guard'", &[])
        .await
        .expect_err("refused");
    assert_eq!(error.code().map(|c| c.code()), Some("42501"));

    // The documented maintenance setting is the restore path: it lets the edit through.
    client
        .batch_execute("SET spot.audit_maintenance = 'on'")
        .await
        .expect("set maintenance");
    let changed = client
        .execute(
            "UPDATE spot_audit_chain SET at = at WHERE site = 'guard'",
            &[],
        )
        .await
        .expect("maintenance edit");
    assert_eq!(changed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_one_byte_tamper_is_found_at_the_exact_seq() {
    let Some(db) = common::db().await else { return };
    let (local, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&db).await,
        "tamper",
        dynamic.clone(),
        CheckpointPolicy { every: 0 },
    );
    for n in 1..=3 {
        chain.append(record(n)).await.expect("append");
    }

    // A superuser with other access flips one byte of the head of seq 2.
    let client = common::raw(&db).await;
    client
        .batch_execute("SET spot.audit_maintenance = 'on'")
        .await
        .expect("set maintenance");
    let changed = client
        .execute(
            "UPDATE spot_audit_chain SET hash = set_byte(hash, 0, (get_byte(hash, 0) + 1) % 256) \
             WHERE site = 'tamper' AND seq = 2",
            &[],
        )
        .await
        .expect("tamper");
    assert_eq!(changed, 1);

    let job = VerifyJob::new(
        common::store(&db).await,
        "tamper",
        dynamic,
        CheckpointPolicy { every: 0 }.every,
        keyring(&local),
    );
    let outcome = job.run().await.expect("run");
    let broken = outcome.broken.expect("a break is reported");
    assert_eq!(broken.seq, 2, "the exact seq");
    assert!(!outcome.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copied_chain_verifies_in_a_fresh_database() {
    let Some(source) = common::db().await else {
        return;
    };
    let Some(target) = common::db().await else {
        return;
    };
    let (local, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&source).await,
        "restore",
        dynamic.clone(),
        CheckpointPolicy { every: 1000 },
    );
    for n in 1..=5 {
        chain.append(record(n)).await.expect("append");
    }
    chain.checkpoint_now(100).await.expect("checkpoint");

    let source_client = common::raw(&source).await;
    let rows = source_client
        .query(
            "SELECT seq, prev_hash, hash, record, at FROM spot_audit_chain WHERE site = 'restore' ORDER BY seq",
            &[],
        )
        .await
        .expect("read the chain");
    let checkpoint = source_client
        .query_opt(
            "SELECT seq, head_hash, at, key_id, signature FROM spot_audit_checkpoint WHERE site = 'restore'",
            &[],
        )
        .await
        .expect("read the checkpoint");

    let dst = common::store(&target).await;
    let dst_client = dst.pool().get().await.expect("connection");
    for row in &rows {
        dst_client
            .execute(
                "INSERT INTO spot_audit_chain (site, seq, prev_hash, hash, record, at) \
                 VALUES ('restore', $1, $2, $3, $4, $5)",
                &[
                    &row.get::<_, i64>(0),
                    &row.get::<_, Vec<u8>>(1),
                    &row.get::<_, Vec<u8>>(2),
                    &row.get::<_, String>(3),
                    &row.get::<_, i64>(4),
                ],
            )
            .await
            .expect("copy a row");
    }
    if let Some(cp) = checkpoint {
        dst_client
            .execute(
                "INSERT INTO spot_audit_checkpoint (site, seq, head_hash, at, key_id, signature) \
                 VALUES ('restore', $1, $2, $3, $4, $5)",
                &[
                    &cp.get::<_, i64>(0),
                    &cp.get::<_, Vec<u8>>(1),
                    &cp.get::<_, i64>(2),
                    &cp.get::<_, String>(3),
                    &cp.get::<_, Vec<u8>>(4),
                ],
            )
            .await
            .expect("copy the checkpoint");
    }

    let job = VerifyJob::new(dst, "restore", dynamic, 1000, keyring(&local));
    let outcome = job.run().await.expect("run");
    assert!(
        outcome.is_ok(),
        "the copied chain did not verify: {:?}",
        outcome.broken
    );
}
