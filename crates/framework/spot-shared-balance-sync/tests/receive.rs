#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Sender authentication: a batch is only settled when the transport's identity, the batch's
//! node, its entries' nodes and the hub's registry all agree.

use spot_audit_chain::PayloadValue;
use spot_offline_identity::{MemoryAuditSink, NodeId as RegistryNodeId, NodeRegistry, TenantId};
use spot_shared_balance::{
    AcceptAndReconcile, BalanceId, Batch, Entry, EntryId, EntryKind, Hub, Millis, NodeId,
    VecQuarantine,
};
use spot_shared_balance_sync::{HeadOffice, NodeIdentity, SyncError, codes, receive_batch};

const OPENING: i64 = 100;

fn node(id: &str) -> NodeId {
    NodeId::new(id)
}

fn registry_with(ids: &[&str]) -> NodeRegistry {
    let mut registry = NodeRegistry::new(TenantId::new("acme").expect("tenant"));
    for id in ids {
        registry.register(RegistryNodeId::new(id).expect("registry node"));
    }
    registry
}

fn hub() -> Hub {
    let mut hub = Hub::new(node("head-office"));
    hub.open_balance(BalanceId::new("card"), OPENING, Millis(0))
        .expect("open");
    hub
}

fn debit(who: &NodeId, seq: u64, amount: i64) -> Entry {
    Entry::new(
        EntryId {
            node: who.clone(),
            seq,
        },
        BalanceId::new("card"),
        EntryKind::Debit,
        amount,
        Millis(10),
    )
    .expect("entry")
}

fn batch(who: &NodeId, entries: Vec<Entry>) -> Batch {
    Batch {
        node: who.clone(),
        entries,
        complete: false,
    }
}

fn policy() -> AcceptAndReconcile {
    AcceptAndReconcile::new(1_000, 4).expect("policy")
}

fn payload_text(record: &spot_audit_chain::AuditRecord, key: &str) -> Option<String> {
    match record.payload.get(key) {
        Some(PayloadValue::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

#[test]
fn a_spoofed_batch_node_is_refused_whole_and_audited_with_both_ids() {
    let registry = registry_with(&["branch-01", "branch-02"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let victim = node("branch-01");
    let attacker = node("branch-02");
    let sent = batch(&victim, vec![debit(&victim, 1, 30)]);

    let error = receive_batch(
        &NodeIdentity::authenticated(attacker.clone()),
        sent,
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a spoofed batch is refused");

    assert_eq!(error.code(), codes::SENDER_MISMATCH);
    assert!(matches!(
        &error,
        SyncError::SenderMismatch { authenticated, claimed, entry: None }
            if authenticated == &attacker && claimed == &victim
    ));
    // Nothing was applied: the card still holds exactly its opening credit.
    let ledger = hub.ledger(&BalanceId::new("card")).expect("ledger");
    assert_eq!(ledger.balance(), OPENING);
    assert_eq!(ledger.entries().len(), 1, "only the opening entry");
    assert!(sink.held.is_empty());
    // The refusal is audited, with both ids.
    assert_eq!(audit.records.len(), 1);
    let record = &audit.records[0];
    assert_eq!(record.kind, codes::BATCH_REJECTED);
    assert_eq!(record.actor, "node:branch-02");
    assert_eq!(
        payload_text(record, "authenticated").as_deref(),
        Some("branch-02")
    );
    assert_eq!(
        payload_text(record, "claimed").as_deref(),
        Some("branch-01")
    );
    assert_eq!(
        payload_text(record, "reason").as_deref(),
        Some(codes::SENDER_MISMATCH)
    );
}

#[test]
fn an_entry_naming_another_node_refuses_the_whole_batch() {
    let registry = registry_with(&["branch-01", "branch-02"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let mine = node("branch-01");
    let theirs = node("branch-02");
    // The first entry is genuine and would apply; the second claims another node. Neither may
    // be applied: the batch is refused whole.
    let sent = batch(&mine, vec![debit(&mine, 1, 20), debit(&theirs, 1, 5)]);

    let error = receive_batch(
        &NodeIdentity::authenticated(mine),
        sent,
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a foreign entry refuses the batch");

    assert_eq!(error.code(), codes::SENDER_MISMATCH);
    assert!(matches!(
        &error,
        SyncError::SenderMismatch { entry: Some(id), claimed, .. }
            if id.node == theirs && claimed == &theirs
    ));
    let ledger = hub.ledger(&BalanceId::new("card")).expect("ledger");
    assert_eq!(ledger.balance(), OPENING);
    assert_eq!(ledger.entries().len(), 1);
    assert_eq!(audit.records.len(), 1);
    assert_eq!(
        payload_text(&audit.records[0], "entry").as_deref(),
        Some("branch-02#1")
    );
}

#[test]
fn a_suspended_node_is_refused() {
    let mut registry = registry_with(&["branch-01"]);
    registry
        .suspend(&RegistryNodeId::new("branch-01").expect("node"), 1)
        .expect("suspend");
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let mine = node("branch-01");

    let error = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        batch(&mine, vec![debit(&mine, 1, 20)]),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a suspended node cannot deliver");

    assert_eq!(error.code(), codes::NODE_NOT_ACTIVE);
    assert!(matches!(
        error,
        SyncError::NodeNotActive {
            state: "suspended",
            ..
        }
    ));
    assert_eq!(
        hub.ledger(&BalanceId::new("card"))
            .expect("ledger")
            .balance(),
        OPENING
    );
    assert_eq!(audit.records.len(), 1);
}

#[test]
fn a_decommissioned_node_is_refused() {
    let mut registry = registry_with(&["branch-01"]);
    registry
        .decommission(&RegistryNodeId::new("branch-01").expect("node"), 2)
        .expect("decommission");
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let mine = node("branch-01");

    let error = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        batch(&mine, vec![debit(&mine, 1, 20)]),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a decommissioned node cannot deliver");

    assert_eq!(error.code(), codes::NODE_NOT_ACTIVE);
    assert!(matches!(
        error,
        SyncError::NodeNotActive {
            state: "decommissioned",
            ..
        }
    ));
}

#[test]
fn an_unregistered_node_is_refused() {
    let registry = registry_with(&["branch-01"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let stranger = node("branch-09");

    let error = receive_batch(
        &NodeIdentity::authenticated(stranger.clone()),
        batch(&stranger, vec![debit(&stranger, 1, 20)]),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("an unregistered node cannot deliver");

    assert_eq!(error.code(), codes::NODE_NOT_REGISTERED);
    assert_eq!(
        hub.ledger(&BalanceId::new("card"))
            .expect("ledger")
            .balance(),
        OPENING
    );
    assert_eq!(audit.records.len(), 1);
}

#[test]
fn a_missing_registry_entry_is_not_an_anonymous_allowance() {
    // A node id the platform's own id type rejects cannot have been registered.
    let registry = registry_with(&["branch-01"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let malformed = node("Branch 01");

    let error = receive_batch(
        &NodeIdentity::authenticated(malformed.clone()),
        batch(&malformed, vec![debit(&malformed, 1, 20)]),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a malformed node id is treated as unregistered");

    assert_eq!(error.code(), codes::NODE_NOT_REGISTERED);
}

#[test]
fn a_refusal_that_cannot_be_audited_still_applies_nothing() {
    let registry = registry_with(&["branch-01"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink {
        fail: true,
        ..MemoryAuditSink::default()
    };
    let mine = node("branch-01");
    let attacker = node("branch-02");

    let error = receive_batch(
        &NodeIdentity::authenticated(attacker),
        batch(&mine, vec![debit(&mine, 1, 20)]),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy(),
            audit: &mut audit,
        },
    )
    .expect_err("a refusal is never turned into an acceptance");

    assert_eq!(error.code(), codes::AUDIT_FAILED);
    assert_eq!(
        hub.ledger(&BalanceId::new("card"))
            .expect("ledger")
            .balance(),
        OPENING
    );
}

#[test]
fn the_same_batch_delivered_twice_applies_once() {
    let registry = registry_with(&["branch-01"]);
    let mut hub = hub();
    let mut sink = VecQuarantine::default();
    let mut audit = MemoryAuditSink::default();
    let mine = node("branch-01");
    let sent = batch(&mine, vec![debit(&mine, 1, 30), debit(&mine, 2, 20)]);
    let policy = policy();

    let first = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        sent.clone(),
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy,
            audit: &mut audit,
        },
    )
    .expect("first delivery");
    assert_eq!(first.applied.len(), 2);
    assert!(first.duplicates.is_empty());

    let second = receive_batch(
        &NodeIdentity::authenticated(mine),
        sent,
        Millis(10),
        &mut sink,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &policy,
            audit: &mut audit,
        },
    )
    .expect("redelivery");

    assert!(second.applied.is_empty(), "nothing is applied twice");
    assert_eq!(second.duplicates.len(), 2);
    assert_eq!(
        hub.ledger(&BalanceId::new("card"))
            .expect("ledger")
            .balance(),
        OPENING - 50,
        "the card is debited exactly once"
    );
    assert!(sink.held.is_empty());
    assert!(audit.records.is_empty(), "an accepted batch is not audited");
}

#[test]
fn every_code_this_crate_raises_is_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    for code in [
        codes::SENDER_MISMATCH,
        codes::NODE_NOT_REGISTERED,
        codes::NODE_NOT_ACTIVE,
        codes::AUDIT_FAILED,
        codes::QUARANTINE_FAILED,
        codes::BATCH_REJECTED,
    ] {
        assert!(catalogue.lookup(code).is_some(), "{code} is catalogued");
    }
}
