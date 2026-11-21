//! The commit service: admit effects into a held region, release at the commit point, discard
//! when the request ends first.

use std::sync::Arc;

use spot_dag::NodeId;
use spot_effect_hold::{
    EffectId, EffectRef, HoldService, ReleasePlan, RequestAdmission, RequestDiscardReport,
    RequestId, RequestTicket,
};

use crate::error::{CommitError, Result};
use crate::region::{RegionPlan, node_source};

/// How a request ended before its commit point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndCause {
    /// The request was cancelled.
    Cancelled,
    /// The request was undone back before the commit point.
    Undone,
}

impl EndCause {
    fn reason(self) -> &'static str {
        match self {
            Self::Cancelled => "request cancelled before its commit point",
            Self::Undone => "request undone before its commit point",
        }
    }
}

/// What [`CommitService::admit`] decides for one effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeAdmission {
    /// The node is outside every held region: the effect flows as it always did.
    Flows,
    /// The effect's class is never held, so it left at once and the node is a point of no return.
    PointOfNoReturn,
    /// The effect is held until the commit point of its region.
    Held(RequestTicket),
}

/// What a commit point did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOutcome {
    /// Effects released, in dispatch order.
    pub released: Vec<EffectId>,
    /// Stale effects that wait for an explicit confirmation.
    pub needs_confirmation: Vec<EffectId>,
    /// Whether a live migration hold deferred the release. Nothing left; retry the commit point
    /// after the migration is verified.
    pub deferred_by_migration: bool,
    /// Whether every effect of the region is out.
    pub completed: bool,
}

/// The commit service of one site's requests.
pub struct CommitService {
    hold: Arc<HoldService>,
    plan: Arc<RegionPlan>,
}

impl std::fmt::Debug for CommitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitService").finish_non_exhaustive()
    }
}

impl CommitService {
    /// A service over the hold and the process's region plan.
    #[must_use]
    pub fn new(hold: Arc<HoldService>, plan: Arc<RegionPlan>) -> Self {
        Self { hold, plan }
    }

    /// Admits one external effect produced at `node` of `request`.
    ///
    /// Outside every held region this changes nothing (`Flows`): the caller dispatches as today.
    /// Inside one, the effect is recorded under the request's scope, tagged with the node so the
    /// region's commit point can find it, or, for a never-held class, passes at once and the node
    /// is reported as a point of no return.
    ///
    /// # Errors
    /// [`CommitError::Hold`] when the hold refuses (a foreign site, an unbounded field).
    pub async fn admit(
        &self,
        request: &RequestId,
        node: &NodeId,
        effect: EffectRef,
    ) -> Result<NodeAdmission> {
        if !self.plan.is_held(node) {
            return Ok(NodeAdmission::Flows);
        }
        let effect = effect.with_source(node_source(node));
        Ok(match self.hold.admit_request(request, &effect).await? {
            RequestAdmission::Proceed { irreversible: true } => NodeAdmission::PointOfNoReturn,
            RequestAdmission::Proceed {
                irreversible: false,
            } => NodeAdmission::Flows,
            RequestAdmission::Held(ticket) => NodeAdmission::Held(ticket),
        })
    }

    /// The commit point `commit` is reached: releases the effects held in its region since the
    /// last commit point, in their original order, with their original idempotency keys.
    ///
    /// Idempotent: calling it again after a crash dispatches only what is not yet out.
    ///
    /// # Errors
    /// [`CommitError::NotCommitPoint`] for a node the plan does not mark, and
    /// [`CommitError::Hold`] when the hold fails (the hold's code tells a dispatch failure from
    /// an unreadable site state). On a dispatch failure nothing after the failed effect is sent.
    pub async fn commit(
        &self,
        request: &RequestId,
        commit: &NodeId,
        plan: ReleasePlan,
        now: i64,
    ) -> Result<CommitOutcome> {
        let sources = self
            .plan
            .sources(commit)
            .ok_or_else(|| CommitError::NotCommitPoint {
                node: commit.to_string(),
            })?;
        let report = self
            .hold
            .release_request(request, plan, Some(&sources), now)
            .await?;
        Ok(CommitOutcome {
            deferred_by_migration: report.deferred_by.is_some(),
            released: report.dispatched,
            needs_confirmation: report.needs_confirmation,
            completed: report.completed,
        })
    }

    /// The request ended before its commit point: every effect it still holds is discarded and
    /// each class's discard policy runs. Effects already released are not touched here: they
    /// left, and the request's undo compensates them.
    ///
    /// # Errors
    /// [`CommitError::Hold`].
    pub async fn end(
        &self,
        request: &RequestId,
        cause: EndCause,
        now: i64,
    ) -> Result<RequestDiscardReport> {
        Ok(self
            .hold
            .discard_request(request, cause.reason(), now)
            .await?)
    }
}
