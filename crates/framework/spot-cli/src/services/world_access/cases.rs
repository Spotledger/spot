//! The cases part of the world (WP-AI-07.T7): the real [`CaseOps`] over a [`CaseDesk`], the seam
//! through which the BPM service's cases are read and unstuck.
//!
//! # Why a seam, and why `from_site` finds nothing yet
//!
//! The BPM service provides `ProcessHost` (start a run, resume, `engine()`) and the process API
//! ports, but no public operation that lists **stuck** cases or unsticks one the way an operator
//! does; the step enqueue the resume loop uses is private to `services/process.rs`. Inventing a
//! stuck-case rule over the engine's stores from this file would be a second definition of
//! "stuck". So the part is written over a small seam, [`CaseDesk`], that a later task implements
//! over the BPM service, and `from_site` answers `None` (readiness says `no cases`). See the
//! report's open questions.
//!
//! # What the part does over a desk
//!
//! - `stuck` lists the stuck cases; `case` reads one (`running` and `done` as the desk reports).
//! - `unstick` runs **as the remedy's actor** (`live_as(actor)`): the desk receives the identity
//!   and applies that user's or agent's own permission, a refusal is a refusal. Without an actor
//!   (`live()` alone) every change is refused: the part never acts as a system principal. A case
//!   that is not stuck is refused.
//! - `restore` is **not supported by the cases port** (an unstuck case has moved on), except to the
//!   state the case already has.
//! - `rehearsal` copies the stuck cases into the in-memory fixture; a rehearsal never reaches the
//!   desk.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`). No case variable or task content is read.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_remedies::fixture::FakeCase;
use spot_remedies::{CaseChange, CaseLife, CaseOps, CaseRef, CaseView, OpError};
use tokio::runtime::Handle;

use super::parts::{CasesPart, PartName};
use crate::services::SiteCtx;
use crate::services::remedies::world::WorldError;

const NOT_SUPPORTED: &str = "not supported by the cases port";

/// A desk call: the answer, or a fixed reason it could not be given (no data in it).
pub type DeskFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The seam over the BPM service's cases.
pub trait CaseDesk: Send + Sync {
    /// The ids of the stuck cases.
    fn stuck(&self) -> DeskFuture<'_, Vec<String>>;

    /// Where `case` stands; `None` when there is no such case.
    fn life<'a>(&'a self, case: &'a str) -> DeskFuture<'a, Option<CaseLife>>;

    /// Unsticks `case` as `actor` (the desk applies `actor`'s own permission) at `now` (unix
    /// seconds, the injected clock's) and answers where the case stands afterwards.
    fn unstick<'a>(&'a self, actor: &'a str, case: &'a str, now: i64) -> DeskFuture<'a, CaseLife>;
}

/// The cases part over a desk.
pub struct DeskCasesPart {
    desk: Arc<dyn CaseDesk>,
    handle: Handle,
}

impl DeskCasesPart {
    /// A part over `desk`; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(desk: Arc<dyn CaseDesk>, handle: Handle) -> Self {
        Self { desk, handle }
    }
}

/// The part for a desk, or `None` when there is no desk or no runtime to block on.
#[must_use]
pub fn from_parts(
    desk: Option<Arc<dyn CaseDesk>>,
    handle: Option<Handle>,
) -> Option<Arc<dyn CasesPart>> {
    Some(Arc::new(DeskCasesPart::new(desk?, handle?)))
}

/// The cases part for this site: `None` until a service provides a [`CaseDesk`] (none does yet).
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn CasesPart>> {
    from_parts(ctx.port::<dyn CaseDesk>(), super::parts::capture_runtime())
}

impl CasesPart for DeskCasesPart {
    fn live(&self) -> Box<dyn CaseOps + '_> {
        Box::new(DeskCaseOps {
            part: self,
            actor: None,
        })
    }

    fn live_as(&self, actor: &str) -> Box<dyn CaseOps + '_> {
        Box::new(DeskCaseOps {
            part: self,
            actor: Some(actor.to_owned()),
        })
    }

    fn rehearsal(&self) -> Result<Box<dyn CaseOps>, WorldError> {
        let stuck = self
            .handle
            .block_on(self.desk.stuck())
            .map_err(|_| PartName::Cases.rehearsal_unavailable())?;
        let mut fake = FakeCase::new();
        for id in stuck {
            fake.seed(&id, CaseLife::Stuck);
        }
        Ok(Box::new(fake))
    }
}

/// The live ops, with the identity a change is made under (if any).
struct DeskCaseOps<'a> {
    part: &'a DeskCasesPart,
    actor: Option<String>,
}

impl DeskCaseOps<'_> {
    fn life(&self, id: &CaseRef) -> Result<Option<CaseLife>, &'static str> {
        self.part.handle.block_on(self.part.desk.life(id.as_str()))
    }
}

impl CaseOps for DeskCaseOps<'_> {
    fn stuck(&self) -> Vec<CaseRef> {
        self.part
            .handle
            .block_on(self.part.desk.stuck())
            .map(|ids| ids.iter().map(|id| CaseRef::new(id)).collect())
            .unwrap_or_default()
    }

    fn case(&self, id: &CaseRef) -> Option<CaseView> {
        let life = self.life(id).ok()??;
        Some(CaseView {
            id: id.clone(),
            life,
        })
    }

    fn unstick(&mut self, id: &CaseRef, now: i64) -> Result<CaseChange, OpError> {
        let op = "case_unstick";
        let Some(actor) = self.actor.as_deref() else {
            return Err(OpError::new(op, "an acting identity is required"));
        };
        let before = self
            .life(id)
            .map_err(|reason| OpError::new(op, reason))?
            .ok_or_else(|| OpError::new(op, &format!("no case `{}`", id.as_str())))?;
        if before != CaseLife::Stuck {
            return Err(OpError::new(
                op,
                &format!("case `{}` is {}, not stuck", id.as_str(), before.as_str()),
            ));
        }
        let after = self
            .part
            .handle
            .block_on(self.part.desk.unstick(actor, id.as_str(), now))
            .map_err(|reason| OpError::new(op, reason))?;
        Ok(CaseChange {
            id: id.clone(),
            before,
            after,
        })
    }

    fn restore(&mut self, change: &CaseChange) -> Result<(), OpError> {
        let op = "case_restore";
        let now = self
            .life(&change.id)
            .map_err(|reason| OpError::new(op, reason))?
            .ok_or_else(|| OpError::new(op, &format!("no case `{}`", change.id.as_str())))?;
        if now == change.before {
            return Ok(());
        }
        Err(OpError::new(op, NOT_SUPPORTED))
    }
}
