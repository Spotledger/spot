//! Hub-side hold filtering (ADR-066 section 5): before a record is queued for a node whose report
//! says it cannot decode the record's topic major, hold it, and hold every later record of the
//! same record key behind it, so the node never applies version 3 and later receives version 2.
//!
//! Precondition: the entries are already scoped to the node. The filter sees only entries the node
//! is entitled to, so a hold never reveals to the node that records exist beyond its scope. A hold
//! carries the record reference only, never content, and a held record is never partially
//! delivered (no redacted stub). Entries of one key arrive with strictly ascending positions across
//! calls; an entry whose position is already queued is treated as the same entry and not queued
//! twice. Persistence of the hold set and the relay itself are out of scope.

use serde::{Deserialize, Serialize};

use crate::error::{HOLD_RETIRED, HOLDS_FULL};
use crate::holds::{Hold, HoldReason, HoldSet, Pending, RecordRef, on_undecodable};
use crate::majors::{Retirement, can_retire};
use crate::model::{Major, NodeReport, NodeState, NodeStatus, Topic};

/// One entry the hub is about to queue for a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    /// Position in the journal stream; delivery order follows it.
    pub position: u64,
    /// The record.
    pub record: RecordRef,
    /// Its event topic.
    pub topic: Topic,
    /// Its schema major.
    pub major: Major,
}

/// What to do with a batch of outgoing entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterResult {
    /// Entries to queue for the node, in input order.
    pub send: Vec<Outgoing>,
    /// Holds created (or, for a resubmitted entry, already present), in input order.
    pub held: Vec<Hold>,
    /// Entries neither sent nor stored because the hold set is full. They are recovered by the
    /// node's catch-up after it upgrades.
    pub unstored: Vec<Outgoing>,
    /// `Some(NodeStatus::TooOld)` when the hold set is full or a major is retired.
    pub node_status: Option<NodeStatus>,
    /// The catalogue code behind `node_status` (`sync_skew.holds_full` or `sync_skew.hold_retired`;
    /// the hub raises `NODE_TOO_OLD`).
    pub condition: Option<&'static str>,
}

/// Filter `entries` (ascending position) for `node`, recording holds in `holds`. Retirement is not
/// considered; see [`filter_outgoing_with_fleet`].
#[must_use]
pub fn filter_outgoing(
    entries: &[Outgoing],
    node: &NodeReport,
    holds: &mut HoldSet,
) -> FilterResult {
    filter(entries, node, holds, None)
}

/// As [`filter_outgoing`], and an undecodable entry whose major is retired for the whole fleet
/// (`can_retire` allows it while an active node exists) gets a permanent [`HoldReason::Retired`]
/// hold: it is returned but not stored, and the node is reported `TooOld`.
#[must_use]
pub fn filter_outgoing_with_fleet(
    entries: &[Outgoing],
    node: &NodeReport,
    holds: &mut HoldSet,
    fleet: &[NodeState],
) -> FilterResult {
    filter(entries, node, holds, Some(fleet))
}

fn key_of(record: &RecordRef) -> (String, String) {
    (record.entity.clone(), record.id.clone())
}

fn retired_for_fleet(entry: &Outgoing, fleet: Option<&[NodeState]>) -> bool {
    fleet.is_some_and(|fleet| {
        fleet.iter().any(|s| s.status == NodeStatus::Active)
            && can_retire(&entry.topic, entry.major, fleet) == Retirement::Allowed
    })
}

fn filter(
    entries: &[Outgoing],
    node: &NodeReport,
    holds: &mut HoldSet,
    fleet: Option<&[NodeState]>,
) -> FilterResult {
    let mut result = FilterResult {
        send: Vec::new(),
        held: Vec::new(),
        unstored: Vec::new(),
        node_status: None,
        condition: None,
    };
    let mut stored = holds.pending_len();
    for entry in entries {
        let key = key_of(&entry.record);
        let queue = holds.queue(&key);
        if let Some(existing) =
            queue.and_then(|q| q.iter().find(|p| p.entry.position == entry.position))
        {
            result.held.push(existing.hold.clone());
            continue;
        }
        let tail = queue.and_then(|q| q.last()).map(|p| p.entry.position);
        let decodable = node.can_decode(&entry.topic, entry.major);
        if decodable && tail.is_none() {
            result.send.push(entry.clone());
            continue;
        }
        if !decodable && retired_for_fleet(entry, fleet) {
            result.held.push(on_undecodable(
                entry.record.clone(),
                HoldReason::Retired {
                    topic: entry.topic.clone(),
                    major: entry.major,
                },
            ));
            result.node_status = Some(NodeStatus::TooOld);
            if result.condition.is_none() {
                result.condition = Some(HOLD_RETIRED);
            }
            continue;
        }
        if stored >= holds.limit() {
            result.unstored.push(entry.clone());
            result.node_status = Some(NodeStatus::TooOld);
            result.condition = Some(HOLDS_FULL);
            continue;
        }
        let reason = match tail {
            Some(blocker) => HoldReason::BehindHeldVersion { blocker },
            None => HoldReason::UnsupportedMajor {
                topic: entry.topic.clone(),
                major: entry.major,
            },
        };
        let hold = on_undecodable(entry.record.clone(), reason);
        holds.queue_mut(key).push(Pending {
            entry: entry.clone(),
            hold: hold.clone(),
        });
        stored += 1;
        result.held.push(hold);
    }
    result
}

/// The node reports a new capability: release what it can now decode. Per record key the entries
/// go in ascending position and stop at the first the node still cannot decode, so a later version
/// is never released before an earlier one (a `BehindHeldVersion` entry follows its blocker). The
/// result is ordered by position across all keys. An entry left at the front of its key's queue is
/// re-labelled `UnsupportedMajor`, since its blocker is gone.
#[must_use]
pub fn release_for_upgrade(node: &NodeReport, holds: &mut HoldSet) -> Vec<Outgoing> {
    let mut released = Vec::new();
    for queue in holds.pending_mut().values_mut() {
        let ready = queue
            .iter()
            .take_while(|p| node.can_decode(&p.entry.topic, p.entry.major))
            .count();
        released.extend(queue.drain(..ready).map(|p| p.entry));
        if let Some(head) = queue.first_mut() {
            head.hold = on_undecodable(
                head.entry.record.clone(),
                HoldReason::UnsupportedMajor {
                    topic: head.entry.topic.clone(),
                    major: head.entry.major,
                },
            );
        }
    }
    holds.pending_mut().retain(|_, queue| !queue.is_empty());
    released.sort_by_key(|e| e.position);
    released
}
