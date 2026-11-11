//! The offline journal and the hub's classification: quarantine, never loss.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use spot_audit_chain::{ChainEntry, PayloadValue, genesis_hash};
use spot_offline_identity::*;

fn stamp(
    seq: u64,
    author: &str,
    assignment: &str,
    tier: Tier,
    epoch: u64,
    age_ms: u64,
) -> OfflineStamp {
    OfflineStamp {
        tenant: tenant(),
        node: node(),
        author: author.to_owned(),
        assignment_id: assignment.to_owned(),
        tier,
        epoch,
        epoch_age_ms: age_ms,
        device_seq: seq,
    }
}

fn clerk(seq: u64) -> OfflineStamp {
    stamp(seq, "clerk", "a-clerk", Tier::Three, 2, 10_000)
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
        let assignments = [("clerk", "a-clerk"), ("payer", "a-payer"), ("sup", "a-sup")]
            .iter()
            .map(|(u, a)| ((*u).to_owned(), (*a).to_owned()))
            .collect();
        Self {
            bounds,
            assignments,
            revocations: vec![],
            tenant: tenant(),
            sender: node(),
        }
    }

    fn view(&self, last_seq: u64) -> HubView<'_> {
        HubView {
            tenant: &self.tenant,
            sender: &self.sender,
            sender_state: NodeState::Active,
            latest_epoch: 2,
            bounds_by_epoch: &self.bounds,
            revocations: &self.revocations,
            assignments: &self.assignments,
            last_device_seq: last_seq,
        }
    }
}

fn reason(v: &Verdict) -> Option<QuarantineReason> {
    match &v.outcome {
        Outcome::Quarantine(r) => Some(*r),
        _ => None,
    }
}

#[test]
fn a_write_under_the_newest_snapshot_within_its_bound_is_accepted() {
    let f = Fixture::new();
    let v = classify(&f.view(0), &[clerk(1)]);
    assert_eq!(v[0].outcome, Outcome::Accepted);
}

#[test]
fn a_write_under_an_older_snapshot_within_its_bound_is_accepted_and_flagged() {
    let f = Fixture::new();
    let old = stamp(1, "clerk", "a-clerk", Tier::Three, 1, 10_000);
    let v = classify(&f.view(0), &[old]);
    assert_eq!(
        v[0].outcome,
        Outcome::AcceptedFlagged(vec![FlagReason::SupersededEpoch])
    );
}

#[test]
fn the_bound_is_exact_per_tier() {
    let f = Fixture::new();
    let at =
        |tier, age| classify(&f.view(0), &[stamp(1, "payer", "a-payer", tier, 2, age)]).remove(0);
    assert_eq!(at(Tier::One, HOUR).outcome, Outcome::Accepted);
    assert_eq!(
        reason(&at(Tier::One, HOUR + 1)),
        Some(QuarantineReason::BeyondBound)
    );
    assert_eq!(at(Tier::Two, 12 * HOUR).outcome, Outcome::Accepted);
    assert_eq!(
        reason(&at(Tier::Two, 12 * HOUR + 1)),
        Some(QuarantineReason::BeyondBound)
    );
    assert_eq!(
        reason(&at(Tier::Three, 24 * HOUR + 1)),
        Some(QuarantineReason::BeyondBound)
    );
}

#[test]
fn a_revoked_author_is_quarantined_and_tier_one_has_its_own_reason() {
    let mut f = Fixture::new();
    f.revocations.push(Revocation::User {
        user: "clerk".into(),
        assignment_id: None,
        effective_epoch: 2,
    });
    f.revocations.push(Revocation::User {
        user: "payer".into(),
        assignment_id: Some("a-payer".into()),
        effective_epoch: 2,
    });
    let stamps = [
        clerk(1),
        stamp(2, "payer", "a-payer", Tier::One, 2, 1_000),
        stamp(3, "payer", "a-payer", Tier::Three, 1, 1_000),
        stamp(4, "sup", "a-sup", Tier::Three, 2, 1_000),
    ];
    let v = classify(&f.view(0), &stamps);
    assert_eq!(reason(&v[0]), Some(QuarantineReason::AuthorRevoked));
    assert_eq!(reason(&v[1]), Some(QuarantineReason::Tier1AfterRevocation));
    assert_eq!(
        reason(&v[2]),
        Some(QuarantineReason::AuthorRevoked),
        "even under the older epoch"
    );
    assert_eq!(
        v[3].outcome,
        Outcome::Accepted,
        "an unrelated author is untouched"
    );
}

#[test]
fn revoking_one_assignment_does_not_quarantine_the_authors_other_assignments() {
    let mut f = Fixture::new();
    f.assignments.insert(("clerk".into(), "a-second".into()));
    f.revocations.push(Revocation::User {
        user: "clerk".into(),
        assignment_id: Some("a-second".into()),
        effective_epoch: 2,
    });
    let v = classify(
        &f.view(0),
        &[clerk(1), stamp(2, "clerk", "a-second", Tier::Three, 2, 1)],
    );
    assert_eq!(v[0].outcome, Outcome::Accepted);
    assert_eq!(reason(&v[1]), Some(QuarantineReason::AuthorRevoked));
}

#[test]
fn claimed_ids_are_checked_against_the_authenticated_connection() {
    let f = Fixture::new();
    let mut other_node = clerk(1);
    other_node.node = NodeId::new("branch-2").unwrap();
    let mut other_tenant = clerk(2);
    other_tenant.tenant = TenantId::new("globex").unwrap();
    let foreign_assignment = stamp(3, "clerk", "a-payer", Tier::Three, 2, 1);
    let unknown_user = stamp(4, "mallory", "a-clerk", Tier::Three, 2, 1);
    let v = classify(
        &f.view(0),
        &[other_node, other_tenant, foreign_assignment, unknown_user],
    );
    assert_eq!(reason(&v[0]), Some(QuarantineReason::NodeMismatch));
    assert_eq!(reason(&v[1]), Some(QuarantineReason::TenantMismatch));
    assert_eq!(reason(&v[2]), Some(QuarantineReason::AssignmentMismatch));
    assert_eq!(reason(&v[3]), Some(QuarantineReason::AssignmentMismatch));
}

#[test]
fn a_suspended_sender_is_quarantined_whatever_it_sends() {
    let f = Fixture::new();
    let mut view = f.view(0);
    view.sender_state = NodeState::Suspended;
    assert_eq!(
        reason(&classify(&view, &[clerk(1)])[0]),
        Some(QuarantineReason::SenderSuspended)
    );
    view.sender_state = NodeState::Decommissioned;
    assert_eq!(
        reason(&classify(&view, &[clerk(1)])[0]),
        Some(QuarantineReason::SenderSuspended)
    );
}

#[test]
fn replayed_and_regressing_sequences_are_quarantined_and_gaps_are_flagged() {
    let f = Fixture::new();
    let v = classify(
        &f.view(5),
        &[clerk(5), clerk(6), clerk(6), clerk(4), clerk(9)],
    );
    assert_eq!(reason(&v[0]), Some(QuarantineReason::SequenceReplay));
    assert_eq!(v[1].outcome, Outcome::Accepted);
    assert_eq!(reason(&v[2]), Some(QuarantineReason::SequenceReplay));
    assert_eq!(reason(&v[3]), Some(QuarantineReason::SequenceReplay));
    assert_eq!(
        v[4].outcome,
        Outcome::AcceptedFlagged(vec![FlagReason::SequenceGap])
    );
}

#[test]
fn an_epoch_the_hub_never_issued_is_quarantined() {
    let f = Fixture::new();
    let future = stamp(1, "clerk", "a-clerk", Tier::Three, 3, 1);
    let zero = stamp(2, "clerk", "a-clerk", Tier::Three, 0, 1);
    let v = classify(&f.view(0), &[future, zero]);
    assert_eq!(reason(&v[0]), Some(QuarantineReason::UnknownEpoch));
    assert_eq!(reason(&v[1]), Some(QuarantineReason::UnknownEpoch));
}

// ---- the journal and the sync into the queue ----

fn journal() -> OfflineJournal<MemoryJournalStore> {
    OfflineJournal::open(&node(), MemoryJournalStore::default()).unwrap()
}

#[derive(Default)]
struct Queue {
    items: Vec<QuarantineItem>,
    fail_on_seq: Option<u64>,
}

impl QuarantinePort for Queue {
    fn quarantine(&mut self, item: &QuarantineItem) -> Result<(), OfflineError> {
        if self.fail_on_seq == Some(item.journal_seq) {
            return Err(OfflineError::JournalStore);
        }
        self.items.push(item.clone());
        Ok(())
    }
}

#[test]
fn the_journal_enforces_a_gapless_device_sequence_and_a_single_node() {
    let mut j = journal();
    assert_eq!(j.next_device_seq(), 1);
    j.append(&clerk(1), 1).unwrap();
    assert_eq!(
        j.append(&clerk(3), 2),
        Err(OfflineError::JournalSequence {
            previous: 1,
            got: 3
        })
    );
    assert_eq!(
        j.append(&clerk(1), 2),
        Err(OfflineError::JournalSequence {
            previous: 1,
            got: 1
        })
    );
    let mut foreign = clerk(2);
    foreign.node = NodeId::new("branch-2").unwrap();
    assert_eq!(j.append(&foreign, 2), Err(OfflineError::StampMalformed));
    j.append(&clerk(2), 2).unwrap();
}

#[test]
fn a_write_whose_journal_entry_cannot_be_stored_is_refused() {
    let mut j = OfflineJournal::open(
        &node(),
        MemoryJournalStore {
            entries: vec![],
            fail: true,
        },
    )
    .unwrap();
    assert_eq!(j.append(&clerk(1), 1), Err(OfflineError::JournalStore));
    assert_eq!(j.next_device_seq(), 1, "nothing was taken");
}

#[test]
fn a_stamp_round_trips_through_its_record() {
    let s = stamp(7, "clerk", "a-clerk", Tier::Two, 3, 99);
    assert_eq!(OfflineStamp::from_record(&s.to_record(5)).unwrap(), s);
}

#[test]
fn a_reopened_journal_continues_and_a_damaged_one_does_not_open() {
    let mut j = journal();
    for n in 1..=3 {
        j.append(&clerk(n), n as i64).unwrap();
    }
    let entries = j.store().entries.clone();
    let mut again = OfflineJournal::open(
        &node(),
        MemoryJournalStore {
            entries: entries.clone(),
            fail: false,
        },
    )
    .unwrap();
    assert_eq!(again.next_device_seq(), 4);
    again.append(&clerk(4), 9).unwrap();

    let mut damaged = entries;
    damaged[1]
        .record
        .payload
        .insert("epoch".into(), PayloadValue::Int(1));
    assert!(matches!(
        OfflineJournal::open(
            &node(),
            MemoryJournalStore {
                entries: damaged,
                fail: false
            }
        ),
        Err(OfflineError::JournalTampered { seq: 2 })
    ));
}

#[test]
fn a_journal_cannot_be_presented_as_another_nodes() {
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    let entries = j.store().entries.clone();
    // The chain starts from the genesis of node branch-1; another node's genesis does not match.
    assert!(verify_entries(0, genesis_hash("branch-1"), &entries).is_ok());
    assert!(verify_entries(0, genesis_hash("branch-2"), &entries).is_err());
}

fn run(
    f: &Fixture,
    entries: &[ChainEntry],
    queue: &mut Queue,
    audit: &mut MemoryAuditSink,
) -> SyncReport {
    sync_journal(
        &f.view(0),
        0,
        genesis_hash("branch-1"),
        entries,
        queue,
        audit,
        1,
    )
}

#[test]
fn sync_accepts_flags_and_quarantines_and_drops_nothing() {
    let mut f = Fixture::new();
    f.revocations.push(Revocation::User {
        user: "payer".into(),
        assignment_id: None,
        effective_epoch: 2,
    });
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    j.append(&stamp(2, "clerk", "a-clerk", Tier::Three, 1, 5), 2)
        .unwrap();
    j.append(&stamp(3, "payer", "a-payer", Tier::One, 2, 5), 3)
        .unwrap();
    j.append(&stamp(4, "clerk", "a-clerk", Tier::One, 2, 2 * HOUR), 4)
        .unwrap();
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &j.store().entries, &mut queue, &mut audit);
    assert_eq!(report.accepted, vec![1]);
    assert_eq!(report.flagged, vec![(2, vec![FlagReason::SupersededEpoch])]);
    assert_eq!(
        report.quarantined,
        vec![
            (3, QuarantineReason::Tier1AfterRevocation),
            (4, QuarantineReason::BeyondBound)
        ]
    );
    assert_eq!(report.ack_through, 4);
    assert_eq!(
        report.highest_device_seq,
        Some(4),
        "every placed write advances the node's device counter"
    );
    assert!(report.unplaced.is_empty());
    assert_eq!(queue.items.len(), 2);
    assert_eq!(queue.items[0].stamp.as_ref().unwrap().author, "payer");
    // Every accepted-but-not-plain and quarantined item has an audit record.
    let kinds: Vec<&str> = audit.records.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "offline_identity.write_quarantined")
            .count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "offline_identity.write_flagged")
            .count(),
        1
    );
    assert!(
        audit
            .records
            .iter()
            .all(|r| r.actor == "offline-identity:node:branch-1"),
        "sync records go to this crate's own actor namespace, never `user:`"
    );
    // A revoked user's actions end up in quarantine and the ledger of what happened is whole.
    assert_eq!(
        report.accepted.len() + report.flagged.len() + report.quarantined.len(),
        4
    );
}

#[test]
fn an_item_the_queue_cannot_take_is_not_acknowledged_and_blocks_the_ack() {
    let mut f = Fixture::new();
    f.revocations.push(Revocation::User {
        user: "payer".into(),
        assignment_id: None,
        effective_epoch: 2,
    });
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    j.append(&stamp(2, "payer", "a-payer", Tier::Three, 2, 5), 2)
        .unwrap();
    j.append(&clerk(3), 3).unwrap();
    let (mut queue, mut audit) = (
        Queue {
            items: vec![],
            fail_on_seq: Some(2),
        },
        MemoryAuditSink::default(),
    );
    let report = run(&f, &j.store().entries, &mut queue, &mut audit);
    assert_eq!(report.unplaced, vec![2]);
    assert_eq!(
        report.ack_through, 1,
        "the node keeps entry 2 and everything after it"
    );
    assert_eq!(
        report.highest_device_seq,
        Some(1),
        "an unplaced write stops the device counter where it stands"
    );
    assert_eq!(report.accepted, vec![1, 3]);
    // Sent again once the queue works.
    queue.fail_on_seq = None;
    let again = run(&f, &j.store().entries, &mut queue, &mut audit);
    assert_eq!(again.ack_through, 3);
    assert_eq!(again.highest_device_seq, Some(3));
}

#[test]
fn a_failing_audit_also_keeps_the_item_unacknowledged() {
    let mut f = Fixture::new();
    f.revocations.push(Revocation::User {
        user: "clerk".into(),
        assignment_id: None,
        effective_epoch: 2,
    });
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    let (mut queue, mut audit) = (
        Queue::default(),
        MemoryAuditSink {
            records: vec![],
            fail: true,
        },
    );
    let report = run(&f, &j.store().entries, &mut queue, &mut audit);
    assert_eq!(report.unplaced, vec![1]);
    assert_eq!(report.ack_through, 0);
}

#[test]
fn a_tampered_journal_quarantines_the_broken_entry_and_everything_after_it() {
    let f = Fixture::new();
    let mut j = journal();
    for n in 1..=4 {
        j.append(&clerk(n), n as i64).unwrap();
    }
    let mut entries = j.store().entries.clone();
    // Someone edits entry 3 to hide who did it.
    entries[2].record.actor = "user:sup".to_owned();
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &entries, &mut queue, &mut audit);
    assert_eq!(report.accepted, vec![1, 2]);
    assert_eq!(
        report.quarantined,
        vec![
            (3, QuarantineReason::JournalTampered),
            (4, QuarantineReason::JournalTampered)
        ]
    );
    assert_eq!(queue.items.len(), 2);
}

#[test]
fn removed_reordered_and_inserted_entries_are_detected() {
    let mut j = journal();
    for n in 1..=4 {
        j.append(&clerk(n), n as i64).unwrap();
    }
    let e = j.store().entries.clone();
    let g = genesis_hash("branch-1");
    let mut removed = e.clone();
    removed.remove(1);
    assert!(verify_entries(0, g, &removed).is_err());
    let mut reordered = e.clone();
    reordered.swap(1, 2);
    assert!(verify_entries(0, g, &reordered).is_err());
    let mut inserted = e.clone();
    inserted.insert(2, e[0].clone());
    assert!(verify_entries(0, g, &inserted).is_err());
    // A batch that continues from a known head verifies from there.
    assert!(verify_entries(2, e[1].hash, &e[2..]).is_ok());
    assert!(verify_entries(2, e[0].hash, &e[2..]).is_err());
}

#[test]
fn a_record_that_is_not_a_stamp_is_quarantined_not_dropped() {
    let f = Fixture::new();
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    let mut entries = j.store().entries.clone();
    let odd = ChainEntry::new(2, entries[0].hash, {
        let mut r = clerk(2).to_record(2);
        r.kind = "something.else".to_owned();
        r
    });
    entries.push(odd);
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &entries, &mut queue, &mut audit);
    assert_eq!(report.accepted, vec![1]);
    assert_eq!(
        report.quarantined,
        vec![(2, QuarantineReason::JournalTampered)]
    );
    assert!(queue.items[0].stamp.is_none());
}

#[test]
fn a_device_gap_is_flagged_and_the_highest_placed_device_seq_is_returned() {
    let f = Fixture::new();
    let g = genesis_hash("branch-1");
    // The node's journal is gapless in journal sequence, but the device counter skipped 2.
    let first = ChainEntry::new(1, g, clerk(1).to_record(1));
    let third = ChainEntry::new(
        2,
        first.hash,
        stamp(3, "clerk", "a-clerk", Tier::Three, 2, 10_000).to_record(2),
    );
    let fourth = ChainEntry::new(
        3,
        third.hash,
        stamp(4, "clerk", "a-clerk", Tier::Three, 2, 10_000).to_record(3),
    );
    let entries = vec![first, third, fourth];
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &entries, &mut queue, &mut audit);
    assert_eq!(report.accepted, vec![1, 3]);
    assert_eq!(report.flagged, vec![(2, vec![FlagReason::SequenceGap])]);
    assert_eq!(
        report.highest_device_seq,
        Some(4),
        "a flagged write is placed, so its device sequence counts"
    );
    assert_eq!(report.ack_through, 3);
}

#[test]
fn an_unreadable_stamp_is_placed_but_never_advances_the_device_counter() {
    let f = Fixture::new();
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    j.append(&stamp(2, "clerk", "a-clerk", Tier::Three, 2, 10_000), 2)
        .unwrap();
    let mut entries = j.store().entries.clone();
    // A well-formed chain whose last record is not a stamp: placed in quarantine, but it
    // carries no device sequence, so the counter stops at the last one it could read.
    let odd = ChainEntry::new(3, entries[1].hash, {
        let mut r = stamp(3, "clerk", "a-clerk", Tier::Three, 2, 10_000).to_record(3);
        r.kind = "something.else".to_owned();
        r
    });
    entries.push(odd);
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &entries, &mut queue, &mut audit);
    assert_eq!(
        report.quarantined,
        vec![(3, QuarantineReason::JournalTampered)]
    );
    assert_eq!(
        report.highest_device_seq,
        Some(2),
        "the counter stops at the last stamp it could read"
    );
    assert_eq!(report.ack_through, 3);
}

#[test]
fn nothing_placed_means_no_device_sequence_at_all() {
    let mut f = Fixture::new();
    f.revocations.push(Revocation::User {
        user: "clerk".into(),
        assignment_id: None,
        effective_epoch: 2,
    });
    let mut j = journal();
    j.append(&clerk(1), 1).unwrap();
    let (mut queue, mut audit) = (
        Queue {
            items: vec![],
            fail_on_seq: Some(1),
        },
        MemoryAuditSink::default(),
    );
    let report = run(&f, &j.store().entries, &mut queue, &mut audit);
    assert_eq!(report.unplaced, vec![1]);
    assert_eq!(report.ack_through, 0);
    assert_eq!(report.highest_device_seq, None);
}

#[test]
fn a_replayed_lower_device_seq_never_lowers_the_highest_placed() {
    let f = Fixture::new();
    let g = genesis_hash("branch-1");
    let first = ChainEntry::new(1, g, clerk(1).to_record(1));
    let high = ChainEntry::new(
        2,
        first.hash,
        stamp(5, "clerk", "a-clerk", Tier::Three, 2, 10_000).to_record(2),
    );
    let replay = ChainEntry::new(
        3,
        high.hash,
        stamp(3, "clerk", "a-clerk", Tier::Three, 2, 10_000).to_record(3),
    );
    let entries = vec![first, high, replay];
    let (mut queue, mut audit) = (Queue::default(), MemoryAuditSink::default());
    let report = run(&f, &entries, &mut queue, &mut audit);
    assert!(report.unplaced.is_empty());
    assert_eq!(
        report.highest_device_seq,
        Some(5),
        "the device counter is a high-water mark and never goes down"
    );
}
