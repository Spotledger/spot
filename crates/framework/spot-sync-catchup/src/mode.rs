//! The mode decision (ADR-066 section 2): incremental, coalesced or snapshot, by cost.
//!
//! There is no offline horizon. A node gone for six months is not refused and not force-restored:
//! the planner compares what each path would cost and picks the cheapest that is correct.

use crate::error::CatchUpError;
use crate::settings::{Settings, Share};
use crate::summary::GapSummary;
use serde::{Deserialize, Serialize};

/// The three ways the hub can bring a node up to date (ADR-066 section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Mode {
    /// Every in-scope event after the applied prefix, in position order.
    Incremental,
    /// The latest image per record since the applied prefix, in dependency order.
    Coalesced,
    /// A scoped snapshot at a consistent position (section 3; produced by another component).
    Snapshot,
}

/// Why a rule fired. Returned as data: a plan carries its reasoning, not just its mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModeReason {
    /// The node is new, rebuilt or reported `ResyncRequired`: only a snapshot is correct.
    NewOrResync,
    /// The gap is at or under the incremental limit and the applied prefix is still in hot
    /// retention, so replaying it in position order is the cheapest correct path.
    SmallGap {
        /// In-scope events in the gap.
        in_scope_event_count: u64,
        /// The limit the gap is under.
        incremental_limit: u64,
    },
    /// The gap holds more events than the incremental limit.
    GapOverLimit {
        /// In-scope events in the gap.
        in_scope_event_count: u64,
        /// The limit the gap passed.
        incremental_limit: u64,
    },
    /// The applied prefix is older than the hot outbox, so the gap can only be served coalesced
    /// from the journal (ADR-061 section 5).
    PrefixOutsideHotRetention {
        /// The node's applied prefix.
        applied_prefix: u64,
        /// The oldest position the hot outbox holds.
        hot_retention_floor: u64,
    },
    /// The coalesced plan would cost more than the configured share of a full scoped snapshot, so
    /// the snapshot is the cheaper path.
    CoalescedCostAboveSnapshotShare {
        /// Bytes the coalesced plan would carry.
        coalesced_bytes: u64,
        /// Bytes a full scoped snapshot would carry.
        snapshot_bytes: u64,
        /// The share in basis points that `coalesced_bytes` passed.
        snapshot_fraction_basis_points: u16,
    },
}

/// A mode and the rules that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeDecision {
    /// The chosen mode.
    pub mode: Mode,
    /// Every rule that fired, in the order they were evaluated. The last one decides the mode.
    pub reasons: Vec<ModeReason>,
}

/// Choose the mode for one node's gap.
///
/// The rules, in this order (ADR-066 section 2):
///
/// 1. new, rebuilt or `ResyncRequired` → [`Mode::Snapshot`];
/// 2. gap at or under the incremental limit **and** the applied prefix still in hot retention →
///    [`Mode::Incremental`];
/// 3. larger than the limit, or older than hot retention → [`Mode::Coalesced`];
/// 4. the estimated coalesced size above the configured share of a full scoped snapshot →
///    [`Mode::Snapshot`].
///
/// The comparison in rule 4 is exact integer arithmetic: `coalesced_bytes * 10,000` against
/// `snapshot_bytes * basis_points` in `u128`, so no float and no overflow decides a plan.
///
/// # Errors
/// `InvalidSettings` or `InconsistentSummary` when either input cannot describe a gap.
pub fn choose_mode(
    summary: &GapSummary,
    settings: &Settings,
) -> Result<ModeDecision, CatchUpError> {
    summary.validate()?;
    settings.validate()?;

    if summary.node_is_new_or_resync {
        return Ok(ModeDecision {
            mode: Mode::Snapshot,
            reasons: vec![ModeReason::NewOrResync],
        });
    }

    let over_limit = summary.in_scope_event_count > settings.incremental_limit;
    let outside_retention = summary.prefix_below_hot_retention();

    if !over_limit && !outside_retention {
        return Ok(ModeDecision {
            mode: Mode::Incremental,
            reasons: vec![ModeReason::SmallGap {
                in_scope_event_count: summary.in_scope_event_count,
                incremental_limit: settings.incremental_limit,
            }],
        });
    }

    let mut reasons = Vec::with_capacity(2);
    if over_limit {
        reasons.push(ModeReason::GapOverLimit {
            in_scope_event_count: summary.in_scope_event_count,
            incremental_limit: settings.incremental_limit,
        });
    }
    if outside_retention {
        reasons.push(ModeReason::PrefixOutsideHotRetention {
            applied_prefix: summary.applied_prefix,
            hot_retention_floor: summary.hot_retention_floor,
        });
    }

    if coalesced_costs_more_than_share(summary, settings.snapshot_fraction) {
        reasons.push(ModeReason::CoalescedCostAboveSnapshotShare {
            coalesced_bytes: summary.estimated_coalesced_bytes,
            snapshot_bytes: summary.estimated_snapshot_bytes,
            snapshot_fraction_basis_points: settings.snapshot_fraction.basis_points(),
        });
        return Ok(ModeDecision {
            mode: Mode::Snapshot,
            reasons,
        });
    }

    Ok(ModeDecision {
        mode: Mode::Coalesced,
        reasons,
    })
}

/// Whether the coalesced plan is strictly above the given share of the snapshot.
fn coalesced_costs_more_than_share(summary: &GapSummary, share: Share) -> bool {
    let coalesced = u128::from(summary.estimated_coalesced_bytes) * u128::from(Share::WHOLE);
    let snapshot = u128::from(summary.estimated_snapshot_bytes) * u128::from(share.basis_points());
    coalesced > snapshot
}
