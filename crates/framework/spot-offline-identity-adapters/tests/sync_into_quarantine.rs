//! The queue and the sync report together: quarantine, never loss, and never a duplicate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use spot_audit_chain::genesis_hash;
use spot_offline_identity::*;
use spot_offline_identity_adapters::{MemoryEnvelopes, OfflineQuarantine};

fn tenant() -> TenantId {
    TenantId::new("acme").unwrap()
}

fn node() -> NodeId {
    NodeId::new("branch-1").unwrap()
}

fn stamp(device_seq: u64, author: &str, assignment: &str, tier: Tier) -> OfflineStamp {
    OfflineStamp {
        tenant: tenant(),
        node: node(),
        author: author.to_owned(),
        assignment_id: assignment.to_owned(),
        tier,
        epoch: 2,
        epoch_age_ms: 10_000,
        device_seq,
    }
}

fn clerk(device_seq: u64) -> OfflineStamp {
    stamp(device_seq, "clerk", "a-clerk", Tier::Three)
}

struct Fixture {
    bounds: BTreeMap<u64, TierBounds>,
    assignments: BTreeSet<(String, String)>,
    revocations: Vec<Revocation>,
    tenant: TenantId,
    sender: NodeId,
}

impl Fixture {
    fn new() -> Self {
        let mut bounds = BTreeMap::new();
        bounds.insert(1, TierBounds::DEFAULT);
        bounds.insert(2, TierBounds::DEFAULT);
        let assignments = [("clerk", "a-clerk"), ("payer", "a-payer")]
            .iter()
            .map(|(u, a)| ((*u).to_owned(), (*a).to_owned()))
            .collect();
        Self {
            bounds,
            assignments,
            revocations: vec![Revocation::User {
                user: "payer".to_owned(),
                assignment_id: None,
                effective_epoch: 2,
            }],
            tenant: tenant(),
            sender: node(),
        }
    }

    fn view(&self) -> HubView<'_> {
        HubView {
            tenant: &self.tenant,
            sender: &self.sender,
            sender_state: NodeState::Active,
            latest_epoch: 2,
            bounds_by_epoch: &self.bounds,
            revocations: &self.revocations,
            assignments: &self.assignments,
            last_device_seq: 0,
        }
    }
}

fn journal() -> OfflineJournal<MemoryJournalStore> {
    OfflineJournal::open(&node(), MemoryJournalStore::default()).unwrap()
}

fn batch() -> Vec<spot_audit_chain::ChainEntry> {
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    j.append(&stamp(2, "payer", "a-payer", Tier::One), 2)
        .unwrap();
    j.append(&clerk(3), 3).unwrap();
    j.store().entries.clone()
}

fn sync_into(
    f: &Fixture,
    queue: &mut OfflineQuarantine<MemoryEnvelopes>,
    audit: &mut MemoryAuditSink,
) -> SyncReport {
    sync_journal(
        &f.view(),
        0,
        genesis_hash("branch-1"),
        &batch(),
        queue,
        audit,
        1,
    )
}

#[test]
fn a_synced_batch_reaches_the_queue_and_a_replay_stores_nothing_new() {
    let f = Fixture::new();
    let mut queue = OfflineQuarantine::open(MemoryEnvelopes::default()).unwrap();
    let mut audit = MemoryAuditSink::default();

    let report = sync_into(&f, &mut queue, &mut audit);
    assert_eq!(report.accepted, vec![1, 3]);
    assert_eq!(
        report.quarantined,
        vec![(2, QuarantineReason::Tier1AfterRevocation)]
    );
    assert_eq!(report.ack_through, 3);
    assert_eq!(
        report.highest_device_seq,
        Some(3),
        "the node may advance its device counter to the last placed write"
    );
    assert_eq!(queue.len(), 1);
    let held = queue.held(&node(), 2).unwrap();
    assert_eq!(held.source.as_str(), "branch-1");
    assert_eq!(held.record_ref.version, 2);
    assert_eq!(
        held.reason_code,
        "offline_identity.quarantine.tier1_after_revocation"
    );
    assert_eq!(
        queue.held_stamp(&node(), 2).unwrap().author,
        "payer",
        "the queue keeps the write itself"
    );

    // The acknowledgement is lost, so the node sends the same journal again.
    let again = sync_into(&f, &mut queue, &mut audit);
    assert_eq!(again.quarantined, report.quarantined);
    assert_eq!(
        queue.len(),
        1,
        "a replayed batch leaves the queue length unchanged"
    );
    assert_eq!(again.highest_device_seq, Some(3));
    assert_eq!(
        queue.store().envelopes.len(),
        1,
        "nothing new was stored either"
    );
}

#[test]
fn a_position_the_queue_holds_with_another_payload_blocks_the_acknowledgement() {
    let f = Fixture::new();
    let mut queue = OfflineQuarantine::open(MemoryEnvelopes::default()).unwrap();
    // Something else claims the same position first.
    queue
        .place(
            &QuarantineItem {
                node: node(),
                journal_seq: 2,
                stamp: Some(stamp(99, "payer", "a-payer", Tier::One)),
                reason: QuarantineReason::Tier1AfterRevocation,
            },
            1,
        )
        .unwrap();
    let mut audit = MemoryAuditSink::default();
    let report = sync_into(&f, &mut queue, &mut audit);
    assert_eq!(
        report.unplaced,
        vec![2],
        "a conflict is not placed, so the write is sent again"
    );
    assert_eq!(
        report.highest_device_seq,
        Some(1),
        "the device counter stops at the last placed write"
    );
    assert_eq!(report.ack_through, 1);
    assert_eq!(queue.len(), 1, "the held payload was not replaced");
    assert_eq!(queue.held_stamp(&node(), 2).unwrap().device_seq, 99);
}

#[test]
fn a_queue_that_cannot_store_keeps_the_write_unplaced() {
    let f = Fixture::new();
    let mut queue = OfflineQuarantine::open(MemoryEnvelopes {
        envelopes: vec![],
        fail: true,
    })
    .unwrap();
    let mut audit = MemoryAuditSink::default();
    let report = sync_into(&f, &mut queue, &mut audit);
    assert_eq!(report.unplaced, vec![2]);
    assert_eq!(report.highest_device_seq, Some(1));
    assert_eq!(report.ack_through, 1);
    assert!(queue.is_empty());
    assert_eq!(
        audit
            .records
            .iter()
            .filter(|r| r.kind == "offline_identity.write_quarantined")
            .count(),
        0,
        "an item the queue did not take is not audited as quarantined"
    );
}
