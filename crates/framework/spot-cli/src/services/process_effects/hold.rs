//! The request-scoped effect hold and its commit points, for the requests of one site.
//!
//! `spot-effect-hold` keeps the durable ledger of held effects; `spot-bpm-commit` decides, on a
//! process's compiled graph, which nodes are held and for which commit point. This module wires
//! them to the site:
//!
//! - **Admit**: an external effect produced at a node inside a held region is recorded under the
//!   request (the idempotency key, the class, the kind, an opaque payload reference; never a
//!   payload) and deferred. Outside every region, or for a class that is never held (a fiscal
//!   transmission), the effect flows and the node is a point of no return.
//! - **Commit**: at the commit point the region's effects are dispatched in their original order,
//!   each with its original key; a repeat after a crash dispatches only what is not yet out
//!   (exactly-once per receipt, proven by the hold's own ledger).
//! - **Discard**: a request cancelled or undone before the commit point discards what it still
//!   holds, and each class's discard policy runs.
//!
//! The region plan of a request comes from the [`PlanSource`] seam (the compiled process the
//! request is pinned to). With no source nothing is held and a request flows exactly as before.
//! The [`Dispatcher`] is the transport that finally sends a released effect; a site with none
//! registered refuses every dispatch, so a held effect stays held (fail closed).

use std::sync::Arc;

use spot_bpm_commit::{CommitOutcome, CommitService, EndCause, NodeAdmission, RegionPlan};
use spot_core::SiteId;
use spot_dag::NodeId;
use spot_effect_hold::{
    AuditSink, Dispatcher, EffectRef, HoldService, LedgerStore, Notifier, PolicyTable, ReleasePlan,
    RequestDiscardReport, RequestId,
};

use super::error::EffError;

/// Where the region plan of a request comes from.
pub trait PlanSource: Send + Sync {
    /// The plan of the process `request` runs. A request with no commit point has a plan that
    /// holds nothing ([`RegionPlan::none`]).
    fn plan(&self, request: &str) -> Arc<RegionPlan>;
}

/// No process declares a commit point: nothing is held.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPlans;

impl PlanSource for NoPlans {
    fn plan(&self, _request: &str) -> Arc<RegionPlan> {
        Arc::new(RegionPlan::none())
    }
}

/// What the hold is built from.
pub struct HoldParts {
    /// The durable ledger of held effects.
    pub ledger: Arc<dyn LedgerStore>,
    /// The transport a released effect goes out through.
    pub dispatcher: Arc<dyn Dispatcher>,
    /// Raises a task for a person when a class discards with a notice.
    pub notifier: Arc<dyn Notifier>,
    /// Where hold transitions are recorded.
    pub audit: Arc<dyn AuditSink>,
    /// The region plan of a request.
    pub plans: Arc<dyn PlanSource>,
}

/// The hold of one site's requests.
pub struct HoldHost {
    service: Arc<HoldService>,
    plans: Arc<dyn PlanSource>,
}

impl HoldHost {
    /// The hold of `site` over `parts`.
    #[must_use]
    pub fn new(site: SiteId, parts: HoldParts) -> Self {
        let service = HoldService::new(
            site,
            parts.ledger,
            Arc::new(PolicyTable::new()),
            parts.audit,
            parts.dispatcher,
            parts.notifier,
        );
        Self {
            service: Arc::new(service),
            plans: parts.plans,
        }
    }

    /// Reads the hold state once at start.
    pub async fn load(&self, now_us: i64) {
        self.service.load(now_us).await;
    }

    fn request_id(request: &str) -> Result<RequestId, EffError> {
        RequestId::new(request).map_err(|_| EffError::ENTRY_INVALID)
    }

    fn commit_service(&self, request: &str) -> CommitService {
        CommitService::new(Arc::clone(&self.service), self.plans.plan(request))
    }

    /// Admits one external effect produced at `node` of `request`.
    ///
    /// # Errors
    /// The hold refused (a bound, a foreign site) or could not record the effect. On an error the
    /// effect was neither sent nor held and the caller must not send it.
    pub async fn admit(
        &self,
        request: &str,
        node: &NodeId,
        effect: EffectRef,
    ) -> Result<NodeAdmission, EffError> {
        let id = Self::request_id(request)?;
        Ok(self
            .commit_service(request)
            .admit(&id, node, effect)
            .await?)
    }

    /// The commit point `node` is reached: releases the effects held in its region, in order.
    ///
    /// # Errors
    /// The node is not a commit point, or the hold failed (a dispatch failure stops the walk;
    /// nothing after it is sent and a repeat resumes).
    pub async fn commit(
        &self,
        request: &str,
        node: &NodeId,
        now_us: i64,
    ) -> Result<CommitOutcome, EffError> {
        let id = Self::request_id(request)?;
        Ok(self
            .commit_service(request)
            .commit(&id, node, ReleasePlan::new(), now_us)
            .await?)
    }

    /// The request ended before its commit point: discards what it still holds.
    ///
    /// # Errors
    /// The hold failed.
    pub async fn discard(
        &self,
        request: &str,
        cause: EndCause,
        now_us: i64,
    ) -> Result<RequestDiscardReport, EffError> {
        let id = Self::request_id(request)?;
        Ok(self.commit_service(request).end(&id, cause, now_us).await?)
    }

    /// How many effects `request` still holds.
    ///
    /// # Errors
    /// The ledger could not be read.
    pub async fn pending(&self, request: &str) -> Result<usize, EffError> {
        let id = Self::request_id(request)?;
        Ok(self.service.request_pending(&id).await?.len())
    }
}
