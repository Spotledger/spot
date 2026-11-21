//! The verification job (idempotent, exact `seq`) and the review digest.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_agent_tiers::DecisionRecord;
use spot_audit_chain::{
    AuditChain, AuditRecord, CheckpointPolicy, CheckpointSigner, KeyRing, LocalSigner,
    PayloadValue, Verifier,
};
use spot_audit_store::{VerifyJob, audit_record_of, digest};

fn doc(n: i64) -> AuditRecord {
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

fn decision(code: &str, at: i64) -> AuditRecord {
    audit_record_of(&DecisionRecord {
        agent: "agent-1".to_owned(),
        action: "change_print_format".to_owned(),
        tier: None,
        decision_code: code.to_owned(),
        decision_reason: "because".to_owned(),
        approval_reference: None,
        at,
    })
}

fn signer() -> (Arc<LocalSigner>, Arc<dyn CheckpointSigner>) {
    let local = Arc::new(LocalSigner::from_seed([5; 32]));
    let dynamic: Arc<dyn CheckpointSigner> = local.clone();
    (local, dynamic)
}

fn keyring(local: &LocalSigner) -> KeyRing {
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).expect("key");
    keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_job_signs_a_checkpoint_and_is_idempotent() {
    let Some(db) = common::db().await else { return };
    let (local, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&db).await,
        "job",
        Arc::clone(&dynamic),
        CheckpointPolicy { every: 0 },
    );
    for n in 1..=3 {
        chain.append(doc(n)).await.expect("append");
    }

    let job = VerifyJob::new(
        common::store(&db).await,
        "job",
        Arc::clone(&dynamic),
        0,
        keyring(&local),
    );
    let first = job.run().await.expect("first run");
    assert!(first.is_ok());
    assert_eq!(first.checked, 3);
    assert_eq!(first.checkpoint.expect("a checkpoint").seq, 3);

    let second = job.run().await.expect("second run");
    assert!(second.is_ok());
    assert!(
        second.checkpoint.is_none(),
        "the range was already checkpointed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_digest_counts_kinds_actors_refusals_and_unmasked() {
    let Some(db) = common::db().await else { return };
    let (local, dynamic) = signer();
    let chain = AuditChain::new(
        common::store(&db).await,
        "digest",
        dynamic,
        CheckpointPolicy { every: 0 },
    );
    chain
        .append(decision("agent.allow", 1))
        .await
        .expect("allow");
    chain
        .append(decision("agent.tier_exceeded", 2))
        .await
        .expect("refusal");
    let mut unmasked = doc(3);
    unmasked.masked = false;
    chain.append(unmasked).await.expect("unmasked");

    let store = common::store(&db).await;
    let keys = keyring(&local);
    let verifier = Verifier::new(&keys);
    let report = digest(&store, &verifier, "digest", 1, u64::MAX)
        .await
        .expect("digest");
    assert_eq!(report.total, 3);
    assert_eq!(report.by_kind.get("agent.decision"), Some(&2));
    assert_eq!(report.by_kind.get("doc.update"), Some(&1));
    assert_eq!(report.by_actor_class.get("agent"), Some(&2));
    assert_eq!(report.by_actor_class.get("user"), Some(&1));
    assert_eq!(report.refusals.len(), 1, "{:?}", report.refusals);
    assert_eq!(report.refusals[0].seq, 2);
    assert_eq!(
        report.refusals[0].code.as_deref(),
        Some("agent.tier_exceeded")
    );
    assert_eq!(report.unmasked.len(), 1);
    assert_eq!(report.unmasked[0].seq, 3);

    let text = report.to_text();
    assert!(text.contains("Audit digest"));
    assert!(text.contains("agent.tier_exceeded"));
    assert_eq!(report.to_json()["total"], 3);
    assert_eq!(report.to_json()["verify"]["ok"], true);

    // The payload of a decision record is only the action and the code.
    let _ = PayloadValue::Text("x".to_owned());
}
