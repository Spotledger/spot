//! The quarantine adapter: idempotent on `(node, journal_seq)`, payload immutable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_offline_identity::{
    NodeId, OfflineError, OfflineStamp, QuarantineItem, QuarantinePort, QuarantineReason, TenantId,
    Tier,
};
use spot_offline_identity_adapters::{
    AdapterError, MemoryEnvelopes, OfflineQuarantine, Placement, QuarantineEnvelopes,
};
use spot_quarantine::State;

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn stamp(device_seq: u64) -> OfflineStamp {
    OfflineStamp {
        tenant: TenantId::new("acme").unwrap(),
        node: node("branch-1"),
        author: "payer".to_owned(),
        assignment_id: "a-payer".to_owned(),
        tier: Tier::One,
        epoch: 2,
        epoch_age_ms: 10_000,
        device_seq,
    }
}

fn item(device_seq: u64) -> QuarantineItem {
    QuarantineItem {
        node: node("branch-1"),
        journal_seq: 7,
        stamp: Some(stamp(device_seq)),
        reason: QuarantineReason::AuthorRevoked,
    }
}

fn queue() -> OfflineQuarantine<MemoryEnvelopes> {
    OfflineQuarantine::open(MemoryEnvelopes::default()).unwrap()
}

#[test]
fn a_replayed_batch_stores_nothing_new_and_returns_ok() {
    let mut q = queue();
    assert!(matches!(
        q.place(&item(3), 1).unwrap(),
        Placement::Stored(_)
    ));
    assert_eq!(q.len(), 1);

    // The node sends the same journal again because the acknowledgement did not arrive.
    assert!(matches!(
        q.place(&item(3), 1).unwrap(),
        Placement::AlreadyHeld(_)
    ));
    assert_eq!(q.len(), 1, "the queue did not grow");
    assert_eq!(
        q.store().envelopes.len(),
        1,
        "nothing new was stored either"
    );

    // Through the port the same replay is Ok, so the write is acknowledged.
    q.quarantine(&item(3)).unwrap();
    assert_eq!(q.len(), 1);
}

#[test]
fn the_same_position_with_a_different_payload_is_an_error() {
    let mut q = queue();
    q.place(&item(3), 1).unwrap();
    assert_eq!(
        q.place(&item(4), 2).unwrap_err(),
        AdapterError::QuarantineConflict {
            node: "branch-1".to_owned(),
            journal_seq: 7,
        }
    );
    // The port reports it as an offline-identity error, so the sync leaves it unplaced.
    assert_eq!(
        q.quarantine(&item(4)).unwrap_err(),
        OfflineError::QuarantineConflict {
            node: "branch-1".to_owned(),
            journal_seq: 7,
        }
    );
    assert_eq!(q.len(), 1, "the held payload was not replaced");
    assert_eq!(q.held(&node("branch-1"), 7).unwrap().id.0, 1);
    assert_eq!(
        q.held_stamp(&node("branch-1"), 7).unwrap().device_seq,
        3,
        "the first payload is the one held"
    );
}

#[test]
fn the_same_position_on_another_node_is_a_different_item() {
    let mut q = queue();
    q.place(&item(1), 1).unwrap();
    let mut other = item(1);
    other.node = node("branch-2");
    assert!(matches!(q.place(&other, 1).unwrap(), Placement::Stored(_)));
    assert_eq!(q.len(), 2);
}

#[test]
fn a_queue_that_cannot_store_is_an_error_so_the_write_is_not_acknowledged() {
    let mut q = OfflineQuarantine::open(MemoryEnvelopes {
        envelopes: vec![],
        fail: true,
    })
    .unwrap();
    assert_eq!(
        q.place(&item(1), 1).unwrap_err(),
        AdapterError::QuarantineWrite
    );
    assert_eq!(
        q.quarantine(&item(1)).unwrap_err(),
        OfflineError::QuarantineStore
    );
    assert!(q.is_empty());
}

#[test]
fn the_item_carries_the_node_the_position_the_reason_and_the_stamp() {
    let mut q = queue();
    q.set_now(1_700_000_000_000_000);
    q.quarantine(&item(5)).unwrap();
    let held = q.held(&node("branch-1"), 7).unwrap();
    assert_eq!(held.source.as_str(), "branch-1");
    assert_eq!(held.seq, 7);
    assert_eq!(held.record_ref.version, 7);
    assert_eq!(held.record_ref.id, "7");
    assert_eq!(
        held.reason_code,
        "offline_identity.quarantine.author_revoked"
    );
    assert_eq!(held.first_seen, 1_700_000_000_000_000);
    assert_eq!(held.state, State::Held);
    assert_eq!(
        q.held_stamp(&node("branch-1"), 7).unwrap(),
        &stamp(5),
        "the payload is the stamp"
    );
}

#[test]
fn reopening_a_queue_rebuilds_its_index_and_stays_idempotent() {
    let mut q = queue();
    q.place(&item(1), 1).unwrap();
    let mut again = OfflineQuarantine::open(q.into_store()).unwrap();
    assert_eq!(again.len(), 1);
    assert!(matches!(
        again.place(&item(1), 2).unwrap(),
        Placement::AlreadyHeld(_)
    ));
    assert_eq!(
        again.place(&item(2), 2).unwrap_err(),
        AdapterError::QuarantineConflict {
            node: "branch-1".to_owned(),
            journal_seq: 7,
        }
    );
}

#[test]
fn a_damaged_store_is_refused_rather_than_silently_reindexed() {
    let mut q = queue();
    q.place(&item(1), 1).unwrap();
    let mut store = q.into_store();
    store.envelopes.push(b"not an envelope".to_vec());
    assert_eq!(
        OfflineQuarantine::open(store).unwrap_err(),
        AdapterError::QuarantineCorrupt
    );
}

#[test]
fn an_envelope_store_that_cannot_be_read_is_refused() {
    struct Broken;
    impl QuarantineEnvelopes for Broken {
        fn load(&self) -> Result<Vec<Vec<u8>>, AdapterError> {
            Err(AdapterError::QuarantineRead)
        }
        fn append(&mut self, _envelope: &[u8]) -> Result<(), AdapterError> {
            Err(AdapterError::QuarantineWrite)
        }
    }
    assert_eq!(
        OfflineQuarantine::open(Broken).unwrap_err(),
        AdapterError::QuarantineRead
    );
}
