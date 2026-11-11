//! The hub side of sync (ADR-070 rule 12.13 i): quarantine, not loss.
//!
//! [`classify`] decides, for each offline write, one of three things:
//!
//! - [`Outcome::Accepted`]: made under the newest snapshot the hub issued to the node, within
//!   the tier's bound;
//! - [`Outcome::AcceptedFlagged`]: within the bound but under an older snapshot (or after a
//!   gap in the sequence); applied and marked for review;
//! - [`Outcome::Quarantine`]: anything else the hub cannot vouch for. It goes to the
//!   quarantine queue (`spot-quarantine`, behind [`QuarantinePort`]) for a person, and is
//!   **never dropped**: if the queue cannot take an item, the item is not acknowledged and the
//!   node sends it again ([`SyncReport::ack_through`]).
//!
//! The checks never trust a claimed id over the authenticated connection: the stamp's tenant
//! and node must equal the tenant and the node that authenticated, and the assignment must be
//! one the hub knows for the author.

use std::collections::{BTreeMap, BTreeSet};

use spot_audit_chain::{ChainEntry, Hash};

use crate::audit::{ACTOR_NODE, AuditSink, actor, audit_record, int, text};
use crate::error::{OfflineError, QuarantineReason};
use crate::node::NodeState;
use crate::snapshot::Revocation;
use crate::stamp::{OfflineStamp, verify_entries};
use crate::types::{NodeId, TenantId, Tier, TierBounds};

/// Why an accepted write is flagged for review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagReason {
    /// The node held an older snapshot than the newest the hub had issued.
    SupersededEpoch,
    /// The device sequence skipped numbers: entries may be missing.
    SequenceGap,
}

/// The verdict on one write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Applied as an ordinary write.
    Accepted,
    /// Applied and flagged.
    AcceptedFlagged(Vec<FlagReason>),
    /// Held for a person.
    Quarantine(QuarantineReason),
}

/// One stamp and its verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The stamp's device sequence.
    pub device_seq: u64,
    /// What to do with it.
    pub outcome: Outcome,
}

/// What the hub knows when a node syncs.
#[derive(Clone, Debug)]
pub struct HubView<'a> {
    /// The tenant of the connection.
    pub tenant: &'a TenantId,
    /// The node the connection authenticated as (mutual TLS).
    pub sender: &'a NodeId,
    /// The sender's state in the registry.
    pub sender_state: NodeState,
    /// The newest snapshot epoch issued to this node.
    pub latest_epoch: u64,
    /// The tier bounds of every snapshot epoch issued to this node. An epoch not in this map
    /// was never issued.
    pub bounds_by_epoch: &'a BTreeMap<u64, TierBounds>,
    /// The hub's revocation records.
    pub revocations: &'a [Revocation],
    /// The `(user, assignment)` pairs the hub holds.
    pub assignments: &'a BTreeSet<(String, String)>,
    /// The highest device sequence already taken from this node.
    pub last_device_seq: u64,
}

fn author_revoked(view: &HubView<'_>, stamp: &OfflineStamp) -> bool {
    view.revocations.iter().any(|r| match r {
        Revocation::User {
            user,
            assignment_id,
            ..
        } => {
            user == &stamp.author
                && assignment_id
                    .as_ref()
                    .is_none_or(|a| a == &stamp.assignment_id)
        }
        Revocation::Node { .. } => false,
    })
}

/// Classifies `stamps` in journal order.
///
/// A revoked author is quarantined whatever the tier or the epoch (the hub cannot tell from
/// the stamp whether the action preceded the revocation, and quarantine only asks a person to
/// look; it loses nothing). A tier-1 action by a revoked author carries its own reason.
#[must_use]
pub fn classify(view: &HubView<'_>, stamps: &[OfflineStamp]) -> Vec<Verdict> {
    let mut running = view.last_device_seq;
    stamps
        .iter()
        .map(|stamp| {
            let (outcome, next) = classify_one(view, stamp, running);
            running = next;
            Verdict {
                device_seq: stamp.device_seq,
                outcome,
            }
        })
        .collect()
}

fn classify_one(view: &HubView<'_>, stamp: &OfflineStamp, running: u64) -> (Outcome, u64) {
    let quarantine = |reason| (Outcome::Quarantine(reason), running);
    if &stamp.tenant != view.tenant {
        return quarantine(QuarantineReason::TenantMismatch);
    }
    if &stamp.node != view.sender {
        return quarantine(QuarantineReason::NodeMismatch);
    }
    if view.sender_state != NodeState::Active {
        return quarantine(QuarantineReason::SenderSuspended);
    }
    if stamp.device_seq <= running {
        return quarantine(QuarantineReason::SequenceReplay);
    }
    let gap = stamp.device_seq > running.saturating_add(1);
    // From here the stamp is the authenticated node's own entry: its place in the sequence
    // is taken whatever the verdict.
    let running = stamp.device_seq;
    let held = |reason| (Outcome::Quarantine(reason), running);
    let Some(bounds) = (stamp.epoch <= view.latest_epoch)
        .then(|| view.bounds_by_epoch.get(&stamp.epoch))
        .flatten()
    else {
        return held(QuarantineReason::UnknownEpoch);
    };
    if !view
        .assignments
        .contains(&(stamp.author.clone(), stamp.assignment_id.clone()))
    {
        return held(QuarantineReason::AssignmentMismatch);
    }
    if author_revoked(view, stamp) {
        return held(if stamp.tier == Tier::One {
            QuarantineReason::Tier1AfterRevocation
        } else {
            QuarantineReason::AuthorRevoked
        });
    }
    if stamp.epoch_age_ms > bounds.get(stamp.tier) {
        return held(QuarantineReason::BeyondBound);
    }
    let mut flags = Vec::new();
    if stamp.epoch < view.latest_epoch {
        flags.push(FlagReason::SupersededEpoch);
    }
    if gap {
        flags.push(FlagReason::SequenceGap);
    }
    if flags.is_empty() {
        (Outcome::Accepted, running)
    } else {
        (Outcome::AcceptedFlagged(flags), running)
    }
}

/// An item for the quarantine queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineItem {
    /// The sending node (the authenticated one).
    pub node: NodeId,
    /// The position in the node's journal chain.
    pub journal_seq: u64,
    /// The stamp, when the journal record could be read as one.
    pub stamp: Option<OfflineStamp>,
    /// Why.
    pub reason: QuarantineReason,
}

/// The quarantine queue of `spot-quarantine`, as this crate sees it. The adapter that stores
/// an item with its immutable payload implements this; it must be idempotent on
/// `(node, journal_seq)` because a refused acknowledgement makes the node send again.
pub trait QuarantinePort {
    /// Stores `item` durably.
    ///
    /// # Errors
    /// [`OfflineError`] when the item is not stored.
    fn quarantine(&mut self, item: &QuarantineItem) -> Result<(), OfflineError>;
}

/// What a sync did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Journal positions applied as ordinary writes.
    pub accepted: Vec<u64>,
    /// Journal positions applied and flagged, with why.
    pub flagged: Vec<(u64, Vec<FlagReason>)>,
    /// Journal positions held in quarantine, with why.
    pub quarantined: Vec<(u64, QuarantineReason)>,
    /// Journal positions the queue or the audit could not take: not acknowledged, sent again.
    pub unplaced: Vec<u64>,
    /// The node may discard its journal through this position; the first unplaced item and
    /// everything after it must be kept.
    pub ack_through: u64,
    /// The highest device sequence number the hub placed (accepted, flagged or quarantined),
    /// over the unbroken prefix of the batch.
    ///
    /// It stops at the first item that could not be placed: an item the queue or the audit
    /// refused is not placed, so the node must not advance its device counter past it and must
    /// send it again. `None` when nothing in the batch was placed. A stamp that could not be
    /// read (a broken chain) carries no device sequence, so it stops the prefix too.
    pub highest_device_seq: Option<u64>,
}

/// Verifies a node's journal batch, classifies it and places every refusal.
///
/// `prev_seq` and `prev_hash` are the chain position the hub already holds for the node
/// (`0` and the genesis hash for the first batch). An entry that fails the chain, and every
/// entry after it, is quarantined as tampered: the stamps of a broken chain cannot be trusted
/// to say who did what.
pub fn sync_journal<Q: QuarantinePort, A: AuditSink>(
    view: &HubView<'_>,
    prev_seq: u64,
    prev_hash: Hash,
    entries: &[ChainEntry],
    queue: &mut Q,
    audit: &mut A,
    at_micros: i64,
) -> SyncReport {
    let intact = match verify_entries(prev_seq, prev_hash, entries) {
        Ok(()) => entries.len(),
        Err(OfflineError::JournalTampered { seq }) => {
            entries.iter().position(|e| e.seq == seq).unwrap_or(0)
        }
        Err(_) => 0,
    };
    let decoded: Vec<Option<OfflineStamp>> = entries
        .iter()
        .take(intact)
        .map(|e| OfflineStamp::from_record(&e.record).ok())
        .collect();
    let readable: Vec<OfflineStamp> = decoded.iter().flatten().cloned().collect();
    let mut verdicts = classify(view, &readable).into_iter();
    let mut report = SyncReport {
        ack_through: prev_seq,
        ..SyncReport::default()
    };
    let mut blocked = false;
    let mut device_known = true;
    for (index, entry) in entries.iter().enumerate() {
        let stamp = decoded.get(index).cloned().flatten();
        let outcome = match (index < intact, &stamp) {
            (true, Some(_)) => verdicts.next().map_or(
                Outcome::Quarantine(QuarantineReason::JournalTampered),
                |v| v.outcome,
            ),
            _ => Outcome::Quarantine(QuarantineReason::JournalTampered),
        };
        let placed = place(view, entry.seq, &stamp, &outcome, queue, audit, at_micros);
        match (&outcome, placed) {
            (Outcome::Accepted, true) => report.accepted.push(entry.seq),
            (Outcome::AcceptedFlagged(f), true) => report.flagged.push((entry.seq, f.clone())),
            (Outcome::Quarantine(r), true) => report.quarantined.push((entry.seq, *r)),
            (_, false) => {
                report.unplaced.push(entry.seq);
                blocked = true;
            }
        }
        if !blocked {
            report.ack_through = entry.seq;
        }
        // The device counter advances only over the unbroken placed prefix, and only while
        // every placed entry carried a readable stamp: a position the hub cannot read has no
        // device sequence, so the counter stops there rather than skipping over it.
        match &stamp {
            Some(s) if !blocked && device_known => {
                let held = report.highest_device_seq.unwrap_or(0);
                report.highest_device_seq = Some(held.max(s.device_seq));
            }
            _ => device_known = false,
        }
    }
    report
}

/// Puts a quarantined item in the queue and audits it. Returns false when the item could not
/// be placed (so it must not be acknowledged).
fn place<Q: QuarantinePort, A: AuditSink>(
    view: &HubView<'_>,
    journal_seq: u64,
    stamp: &Option<OfflineStamp>,
    outcome: &Outcome,
    queue: &mut Q,
    audit: &mut A,
    at_micros: i64,
) -> bool {
    let (kind, reason) = match outcome {
        Outcome::Accepted => return true,
        Outcome::AcceptedFlagged(flags) => {
            let flagged = audit_record(
                concat!("offline_identity", ".write_flagged"),
                &actor(ACTOR_NODE, view.sender.as_str()),
                at_micros,
                &[
                    ("journal_seq", int(journal_seq)),
                    ("flags", int(flags.len() as u64)),
                ],
            );
            return audit.record(flagged).is_ok();
        }
        Outcome::Quarantine(reason) => (concat!("offline_identity", ".write_quarantined"), *reason),
    };
    let item = QuarantineItem {
        node: view.sender.clone(),
        journal_seq,
        stamp: stamp.clone(),
        reason,
    };
    if queue.quarantine(&item).is_err() {
        return false;
    }
    let record = audit_record(
        kind,
        &actor(ACTOR_NODE, view.sender.as_str()),
        at_micros,
        &[
            ("journal_seq", int(journal_seq)),
            ("reason", text(reason.code())),
        ],
    );
    audit.record(record).is_ok()
}
