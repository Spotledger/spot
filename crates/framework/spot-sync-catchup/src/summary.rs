//! The input to the mode decision: what the hub knows about one node's gap (ADR-066 section 2).
//!
//! The summary is handed to the planner by the session handshake and the store; the planner reads
//! nothing itself. It holds positions and estimates only, never an event or a record image.

use crate::error::CatchUpError;
use crate::event::Position;
use serde::{Deserialize, Serialize};

/// How many in-scope events of each ADR-061 section 2 class the gap holds.
///
/// Telemetry has no field: telemetry is never written to the outbox, so it can never be planned
/// (ADR-061 section 2). The three classes here are the ones a node can receive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassCounts {
    /// Full-state events, one per record per transaction.
    pub state: u64,
    /// Business and app facts.
    pub domain: u64,
    /// System facts for proof and rebuild, sent to a node only when addressed to it.
    pub audit: u64,
}

impl ClassCounts {
    /// Every counted event, summed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.state
            .saturating_add(self.domain)
            .saturating_add(self.audit)
    }
}

/// One node's gap: where it is, where the stream ends, what is in scope and what each mode would
/// cost.
///
/// Every estimate is the caller's: the planner compares them and never measures anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapSummary {
    /// The node's applied prefix: everything at or below it is already applied.
    pub applied_prefix: Position,
    /// The current end of the stream the node reads.
    pub stream_end: Position,
    /// The oldest position the hot outbox still holds; below it, history lives only in the journal.
    pub hot_retention_floor: Position,
    /// In-scope events after `applied_prefix` and up to `stream_end`.
    pub in_scope_event_count: u64,
    /// Those events split by class.
    pub class_counts: ClassCounts,
    /// Distinct records the coalesced plan would carry.
    pub estimated_coalesced_records: u64,
    /// Bytes the coalesced plan would carry.
    pub estimated_coalesced_bytes: u64,
    /// Bytes a full scoped snapshot at `stream_end` would carry.
    pub estimated_snapshot_bytes: u64,
    /// The node is new, rebuilt, or reported `ResyncRequired` (ADR-053, ADR-066 section 4).
    pub node_is_new_or_resync: bool,
}

impl GapSummary {
    /// Whether the summary describes a possible gap.
    ///
    /// # Errors
    /// `InconsistentSummary` when the positions are not an ordered range, or the class counts
    /// claim more events than the gap holds.
    pub fn validate(&self) -> Result<(), CatchUpError> {
        if self.applied_prefix > self.stream_end {
            return Err(CatchUpError::InconsistentSummary {
                reason: "the applied prefix is beyond the stream end",
            });
        }
        if self.hot_retention_floor > self.stream_end {
            return Err(CatchUpError::InconsistentSummary {
                reason: "the hot retention floor is beyond the stream end",
            });
        }
        if self.class_counts.total() > self.in_scope_event_count {
            return Err(CatchUpError::InconsistentSummary {
                reason: "the class counts add up to more events than the gap holds",
            });
        }
        Ok(())
    }

    /// The gap's width in stream positions. Zero when the node is caught up.
    #[must_use]
    pub fn width(&self) -> u64 {
        self.stream_end.saturating_sub(self.applied_prefix)
    }

    /// Whether the applied prefix is older than what the hot outbox still holds, so history has to
    /// come from the journal (ADR-061 section 5).
    #[must_use]
    pub fn prefix_below_hot_retention(&self) -> bool {
        self.applied_prefix < self.hot_retention_floor
    }
}
