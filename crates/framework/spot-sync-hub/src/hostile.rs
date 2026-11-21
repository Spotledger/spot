//! The three hostile-node bounds, wired where the hub's own values live (ADR-062 s3 to s5,
//! ADR-065 s3, s7 and s8, ADR-066 s1 to s3): stamped documents and catch-up planning. The ack
//! bound is in `HubRelay::on_ack_at`.

use crate::error::HubError;
use crate::relay::HubRelay;
use crate::traits::{GapSource, HubStream, SendQueue};
use spot_sync_catchup::{
    CatchUpPlan, GapSummary, References, Settings, TopicDeclarations, plan, scope_for_node,
};
use spot_sync_closure::ImageSource;
use spot_sync_freshness::{
    Finding, FreshnessError, SyncStamp, find_violations_clamped, reject_audit,
};

/// What the hub decided about a stamped document it accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StampDecision {
    /// The stale-document findings, by mandatory-event position. Empty when the node posted
    /// under every rule that was already in force.
    pub findings: Vec<Finding>,
}

/// A planned catch-up for one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpOutcome {
    /// The plan, built from the destination's scoped gap only.
    pub plan: CatchUpPlan,
    /// Audit events scoping removed because they were addressed to another node.
    pub audit_dropped: usize,
}

impl<Q: SendQueue, S: HubStream, I: ImageSource> HubRelay<Q, S, I> {
    /// A node submitted a document with a sync stamp, received by the hub at
    /// `hub_received_at_millis` (the host's clock port). `node` is the authenticated session's
    /// node id, not the id written in the stamp.
    ///
    /// The stamp's claimed offset is bounded by the relay's [`Bounds`](spot_sync_freshness::Bounds)
    /// against the hub's receipt time and the posting time is built from that clamp
    /// (`find_violations_clamped`); the stamp's own `hub_time` and `hub_offset_ms` are never read
    /// for a decision. Findings are made against the mandatory events the ledger holds.
    ///
    /// # Errors
    /// `StampRejected` when the stamp cannot be placed on the hub timeline (an audit note goes
    /// to the audit seam and the document must not enter the stream); `UnknownDestination`;
    /// `InvalidConfig` when no bounds or no ledger is set (fail closed); `Freshness` for any
    /// other freshness failure (an internal error, never "no findings").
    pub fn on_stamped_document(
        &mut self,
        node: &str,
        document: &str,
        stamp: &SyncStamp,
        hub_received_at_millis: i64,
    ) -> Result<StampDecision, HubError> {
        if !self.dests.contains_key(node) {
            return Err(HubError::UnknownDestination {
                dest: node.to_owned(),
            });
        }
        let Some(bounds) = self.bounds else {
            return Err(HubError::InvalidConfig {
                reason: "no stamp bounds are set",
            });
        };
        let Some(ledger) = self.ledger.as_ref() else {
            return Err(HubError::InvalidConfig {
                reason: "no acknowledgement ledger is set",
            });
        };
        // The node id is the hub's: a stamp cannot name another node's identity.
        let mut stamp = stamp.clone();
        node.clone_into(&mut stamp.node);
        let events = ledger.mandatory_events(&self.cfg.tenant);
        match find_violations_clamped(document, &stamp, hub_received_at_millis, &bounds, &events) {
            Ok(findings) => Ok(StampDecision { findings }),
            Err(FreshnessError::StampUnrepresentable) => {
                self.audit
                    .stamp_rejected(reject_audit(&stamp, hub_received_at_millis));
                Err(HubError::StampRejected {
                    node: node.to_owned(),
                })
            }
            Err(other) => Err(HubError::Freshness(other)),
        }
    }

    /// Plan the catch-up of `dest` after a gap. The gap is read through `gap`, bounded by the
    /// hub's own stream head, scoped for `dest` (audit events addressed to another node are
    /// removed and counted) and only then handed to the planner.
    ///
    /// A summary whose applied prefix or stream end is above the hub's head is refused and marks
    /// the destination suspect: the summary is built from node-reported values.
    ///
    /// # Errors
    /// `UnknownDestination`, `AckBeyondHead`, `StreamFailed`, or the planner's refusal
    /// (`CatchUp`).
    pub fn catch_up(
        &mut self,
        dest: &str,
        summary: &GapSummary,
        gap: &impl GapSource,
        settings: &Settings,
        topics: &impl TopicDeclarations,
        refs: &impl References,
    ) -> Result<CatchUpOutcome, HubError> {
        if !self.dests.contains_key(dest) {
            return Err(HubError::UnknownDestination {
                dest: dest.to_owned(),
            });
        }
        let head = self.stream.head()?;
        let claimed = summary.applied_prefix.max(summary.stream_end);
        if claimed > head {
            self.mark_suspect(dest);
            return Err(HubError::AckBeyondHead {
                dest: dest.to_owned(),
                claimed,
                head,
            });
        }
        let events = gap.events_between(summary.applied_prefix, summary.stream_end)?;
        let scoped = scope_for_node(&events, dest, topics);
        let audit_dropped = scoped.dropped_audit();
        let planned = plan(&scoped, summary, settings, topics, refs)?;
        self.audit_dropped += audit_dropped;
        Ok(CatchUpOutcome {
            plan: planned,
            audit_dropped,
        })
    }
}
