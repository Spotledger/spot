//! Which snapshots to keep: the last 7 daily and 4 weekly, the newest of every generation, and
//! never the only verified one. A pure plan: nothing is deleted here.
//!
//! The journal store refuses to delete anything under `snapshots/` (`StoreError::DeleteRefused`),
//! so the host reports the plan (audited) and a provider-level prune carries it out later; see
//! the task report.

use std::collections::BTreeSet;

use spot_journal_rebuild_plan::SnapshotInfo;

const DAY_MILLIS: i64 = 86_400_000;
const WEEK_MILLIS: i64 = 7 * DAY_MILLIS;

/// How many daily and weekly snapshots to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// The newest snapshot of each of this many most recent days.
    pub daily: usize,
    /// The newest snapshot of each of this many most recent weeks.
    pub weekly: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            daily: 7,
            weekly: 4,
        }
    }
}

/// A snapshot's identity.
pub type SnapshotId = (u64, u64);

/// What to keep and what may go.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RetentionPlan {
    /// `(generation, position)` of every snapshot kept.
    pub keep: Vec<SnapshotId>,
    /// `(generation, position)` of every snapshot that may be pruned.
    pub prune: Vec<SnapshotId>,
}

fn id_of(info: &SnapshotInfo) -> SnapshotId {
    (info.generation, info.position)
}

/// The plan over `infos` (any order).
#[must_use]
pub fn plan(infos: &[SnapshotInfo], retention: Retention) -> RetentionPlan {
    let mut sorted: Vec<&SnapshotInfo> = infos.iter().collect();
    // Newest first.
    sorted.sort_by_key(|info| {
        std::cmp::Reverse((info.taken_at_millis, info.generation, info.position))
    });
    let mut keep: BTreeSet<SnapshotId> = BTreeSet::new();
    let mut days = BTreeSet::new();
    let mut weeks = BTreeSet::new();
    let mut generations = BTreeSet::new();
    let mut verified_kept = false;
    for info in &sorted {
        let day = info.taken_at_millis.div_euclid(DAY_MILLIS);
        let week = info.taken_at_millis.div_euclid(WEEK_MILLIS);
        if days.len() < retention.daily && days.insert(day) {
            keep.insert(id_of(info));
        }
        if weeks.len() < retention.weekly && weeks.insert(week) {
            keep.insert(id_of(info));
        }
        // The newest one per generation.
        if generations.insert(info.generation) {
            keep.insert(id_of(info));
        }
    }
    // Never drop the only verified snapshot: the newest verified one is always kept.
    for info in &sorted {
        if info.verified {
            keep.insert(id_of(info));
            verified_kept = true;
            break;
        }
    }
    let _ = verified_kept;
    let mut out = RetentionPlan::default();
    for info in &sorted {
        if keep.contains(&id_of(info)) {
            out.keep.push(id_of(info));
        } else {
            out.prune.push(id_of(info));
        }
    }
    out
}
