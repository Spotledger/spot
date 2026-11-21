//! Re-execution: the seam a replay runs each sampled operation through.
//!
//! A [`ReplayRunner`] is the write half of a replay. For one sampled operation it runs the
//! operation through the document engine **on the clone only**, and returns the canonical
//! [`Outcome`] the new release produced. The [`ReplayRequest`] fixes everything the run must use:
//!
//! - the [`CloneRef`] — the only database a runner may write to;
//! - `at`, the instant the original operation happened at, which the runner fixes its clock to
//!   (the engine reads no clock of its own for business time);
//! - the original [`Principal`], so the operation runs under the rights it ran under;
//! - the [`EffectHold`], which every external effect must pass through in `Held` mode.
//!
//! The engine owns the *shape*, not the engine behind the seam: a host supplies a runner over
//! `spot-document`'s engine; the tests supply a toy configuration over the storage fake.

use spot_app_api::prelude::{OffsetDateTime, SiteId};

use crate::BoxFuture;
use crate::clone::CloneRef;
use crate::error::Result;
use crate::hold::EffectHold;
use crate::journal::{JournalEntry, Principal, ReplayOperation};
use crate::outcome::Outcome;

/// One operation to re-execute, with everything the run must use.
pub struct ReplayRequest<'a> {
    /// The clone: the only database the runner may write to.
    pub clone: &'a CloneRef,
    /// The site.
    pub site: &'a SiteId,
    /// The instant the original operation happened at; the runner fixes its clock to it.
    pub at: OffsetDateTime,
    /// The principal the operation ran as.
    pub principal: &'a Principal,
    /// The journal entry: the operation, its DocType and its record.
    pub entry: &'a JournalEntry,
    /// The effect hold every external effect must pass through.
    pub hold: &'a EffectHold,
}

impl ReplayRequest<'_> {
    /// The operation to re-execute.
    #[must_use]
    pub fn operation(&self) -> &ReplayOperation {
        &self.entry.operation
    }

    /// The DocType the operation is about.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.entry.doctype
    }
}

/// What re-executes one sampled operation on the clone.
pub trait ReplayRunner: Send + Sync {
    /// Runs the request's operation through the document engine on the clone and returns the
    /// canonical outcome the new release produced.
    ///
    /// A refusal (the new release rejects the operation) is an [`Outcome`] with
    /// [`Status::Rejected`](crate::Status::Rejected), not an error; an error means the run could
    /// not be completed and aborts the replay.
    fn run<'a>(&'a self, request: ReplayRequest<'a>) -> BoxFuture<'a, Result<Outcome>>;
}
