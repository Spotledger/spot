//! The result log: the tamper-evident chain the customer can verify.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::future::Future;
use std::sync::Arc;

use spot_agent_core::{
    AuditExecutor, ChainResultLog, DecisionRecord, MemoryResultLog, Refusal, ResultLog,
};
use spot_audit_chain::{
    AuditChain, AuditStore, CheckpointPolicy, InMemoryStore, KeyRing, LocalSigner, Verifier,
};

/// A blocking executor over the audit chain's async API.
struct BlockOn;

impl AuditExecutor for BlockOn {
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, Refusal> {
        Ok(futures::executor::block_on(future))
    }
}

fn record(at: i64, outcome: &str) -> DecisionRecord {
    DecisionRecord {
        actor: "agent:agent-1".to_owned(),
        command_id: format!("command-{at}"),
        command_digest: [at as u8; 32],
        op: "server.service.restart".to_owned(),
        signers: vec!["aa".repeat(16)],
        outcome: outcome.to_owned(),
        code: None,
        at,
    }
}

#[test]
fn the_memory_log_links_and_verifies() {
    let log = MemoryResultLog::new("agent-1");
    assert!(log.verify());
    log.append(&record(10, "execute")).unwrap();
    log.append(&record(11, "executed")).unwrap();
    assert_eq!(log.entries().unwrap().len(), 2);
    assert!(log.verify());
}

#[test]
fn the_chain_log_is_verifiable_by_the_customer_view() {
    let signer = LocalSigner::from_seed([7u8; 32]);
    let mut key_ring = KeyRing::new();
    let key_id = key_ring.add(&signer.public_key()).unwrap();
    let store = InMemoryStore::new();
    let policy = CheckpointPolicy { every: 1 };
    let chain = AuditChain::new(store, "agent-1", Arc::new(signer), policy);
    let log = ChainResultLog::new(chain, BlockOn);

    log.append(&record(10, "execute")).unwrap();
    log.append(&record(11, "executed")).unwrap();
    assert_eq!(log.site(), "agent-1");

    // The customer-side verifier recomputes every hash and link and checks the latest
    // signed checkpoint against the chain; the record kinds are visible, the payloads are
    // not needed to verify.
    let verifier = Verifier::new(&key_ring);
    let report = futures::executor::block_on(verifier.verify_range(
        log.chain().store(),
        "agent-1",
        1,
        u64::MAX,
    ))
    .unwrap();
    assert!(report.is_ok(), "the chain verifies: {:?}", report.failures);
    assert_eq!(report.checked, 2);

    let checkpoint = futures::executor::block_on(log.chain().head())
        .unwrap()
        .unwrap();
    let latest = futures::executor::block_on(log.chain().store().latest_checkpoint("agent-1"))
        .unwrap()
        .unwrap();
    assert_eq!(latest.seq, checkpoint.seq);
    assert_eq!(verifier.check_checkpoint("agent-1", &latest), None);
    assert_eq!(latest.key_id, key_id);
}
