//! Recording a run: the [`RunRecorder`] trait the apply and rehearsal engines call, and the
//! chain-backed implementation.
//!
//! The apply and rehearsal crates depend on this trait (never the other way round, so there is
//! no cycle); the implementation here appends each lifecycle event to the tamper-evident audit
//! chain. [`RunRecorder::begin`] is the **only** source of a [`RunHandle`], so an apply that
//! presents no handle cannot have a recorded run — the hole this crate closes.

use std::collections::BTreeSet;
use std::sync::Mutex;

use futures::future::BoxFuture;
use spot_audit_chain::{AuditChain, AuditStore};

use crate::error::MigrationAuditError;
use crate::events::{
    ApplyEvent, ApprovalEvent, FinishEvent, RehearsalEvent, RunSpec, applied_record,
    approval_record, finished_record, planned_record, rehearsal_record,
};

/// The token that a recorded run exists.
///
/// A `RunHandle` has no public constructor: the only way to obtain one is
/// [`RunRecorder::begin`], which appends the run's first event to the chain before returning it.
/// The apply entry point takes a `RunHandle` (and a [`Permit`](crate::Permit)) and nothing else,
/// so a direct step runner with no recorded run cannot be reached through the public API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHandle {
    id: crate::run::RunId,
    site: String,
    actor: String,
}

impl RunHandle {
    /// The run's id.
    #[must_use]
    pub fn id(&self) -> &crate::run::RunId {
        &self.id
    }

    /// The site the run belongs to.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The identity running the migration.
    #[must_use]
    pub fn actor(&self) -> &str {
        &self.actor
    }
}

/// Where a run's lifecycle events go.
///
/// The apply and rehearsal engines call these; each records one event in order. Every method is
/// fallible and **fails closed**: an event that cannot be recorded is an error the caller must
/// not ignore, because the run would otherwise be untraceable.
pub trait RunRecorder: Send + Sync {
    /// Begins a run and returns its handle, after recording the plan and approvals.
    ///
    /// # Errors
    /// [`MigrationAuditError::DuplicateRun`] when this id already began on this recorder,
    /// [`MigrationAuditError::Chain`] when the chain store fails, or
    /// [`MigrationAuditError::RecordInvalid`] when the event is malformed.
    fn begin<'a>(&'a self, spec: RunSpec) -> BoxFuture<'a, Result<RunHandle, MigrationAuditError>>;

    /// Records one approval (or rejection) of the plan.
    ///
    /// # Errors
    /// As [`RunRecorder::begin`], and [`MigrationAuditError::WrongSite`] when the handle is for
    /// another site.
    fn record_approval<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApprovalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>>;

    /// Records the rehearsal report and its verdict.
    ///
    /// # Errors
    /// As [`RunRecorder::record_approval`].
    fn record_rehearsal<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: RehearsalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>>;

    /// Records that apply ran, with its journal.
    ///
    /// # Errors
    /// As [`RunRecorder::record_approval`].
    fn record_apply<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApplyEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>>;

    /// Records the terminal outcome.
    ///
    /// # Errors
    /// [`MigrationAuditError::BadOutcome`] when the outcome is not terminal, and as
    /// [`RunRecorder::record_approval`].
    fn finish<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: FinishEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>>;
}

/// Whether `actor` is `system`, `user:<id>` or `agent:<id>` with a non-empty id, the audit
/// chain's actor form.
fn actor_ok(actor: &str) -> bool {
    actor == "system"
        || actor
            .strip_prefix("user:")
            .or_else(|| actor.strip_prefix("agent:"))
            .is_some_and(|id| !id.is_empty())
}

/// A [`RunRecorder`] over one site's tamper-evident audit chain.
///
/// One recorder per site, because the chain is per site. It reads no clock and no randomness:
/// every event carries the time the caller passed.
pub struct ChainRunRecorder<S: AuditStore> {
    chain: AuditChain<S>,
    begun: Mutex<BTreeSet<crate::run::RunId>>,
}

impl<S: AuditStore> ChainRunRecorder<S> {
    /// A recorder over `chain`.
    #[must_use]
    pub fn new(chain: AuditChain<S>) -> Self {
        Self {
            chain,
            begun: Mutex::new(BTreeSet::new()),
        }
    }
    /// The site this recorder writes.
    #[must_use]
    pub fn site(&self) -> &str {
        self.chain.site()
    }

    /// The chain store, so a [`ChainRunStore`](crate::ChainRunStore) can read the same chain.
    #[must_use]
    pub fn store(&self) -> &S {
        self.chain.store()
    }

    /// The audit chain itself.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }

    fn ensure_site(&self, handle: &RunHandle) -> Result<(), MigrationAuditError> {
        if handle.site == self.chain.site() {
            Ok(())
        } else {
            Err(MigrationAuditError::WrongSite {
                run_site: handle.site.clone(),
                site: self.chain.site().to_owned(),
            })
        }
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeSet<crate::run::RunId>>, MigrationAuditError> {
        self.begun.lock().map_err(|_| MigrationAuditError::Chain {
            message: "the begun-runs lock is poisoned".to_owned(),
        })
    }
}

impl<S: AuditStore> RunRecorder for ChainRunRecorder<S> {
    fn begin<'a>(&'a self, spec: RunSpec) -> BoxFuture<'a, Result<RunHandle, MigrationAuditError>> {
        Box::pin(async move {
            if !actor_ok(&spec.actor) {
                return Err(MigrationAuditError::BadActor {
                    message: "the actor is not system, user:<id> or agent:<id>".to_owned(),
                });
            }
            // Reserve the id before appending, so two concurrent begins cannot both pass.
            if !self.lock()?.insert(spec.id.clone()) {
                return Err(MigrationAuditError::DuplicateRun {
                    id: spec.id.to_string(),
                });
            }
            if let Err(error) = self.chain.append(planned_record(&spec)).await {
                self.lock()?.remove(&spec.id);
                return Err(error.into());
            }
            Ok(RunHandle {
                id: spec.id,
                site: self.chain.site().to_owned(),
                actor: spec.actor,
            })
        })
    }

    fn record_approval<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApprovalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        Box::pin(async move {
            self.ensure_site(handle)?;
            self.chain
                .append(approval_record(&handle.actor, &handle.id, &event))
                .await?;
            Ok(())
        })
    }

    fn record_rehearsal<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: RehearsalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        Box::pin(async move {
            self.ensure_site(handle)?;
            self.chain
                .append(rehearsal_record(&handle.actor, &handle.id, &event))
                .await?;
            Ok(())
        })
    }

    fn record_apply<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApplyEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        Box::pin(async move {
            self.ensure_site(handle)?;
            self.chain
                .append(applied_record(&handle.actor, &handle.id, &event))
                .await?;
            Ok(())
        })
    }

    fn finish<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: FinishEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        Box::pin(async move {
            self.ensure_site(handle)?;
            if !event.outcome.is_terminal() {
                return Err(MigrationAuditError::BadOutcome {
                    outcome: event.outcome.as_str().to_owned(),
                });
            }
            self.chain
                .append(finished_record(&handle.actor, &handle.id, &event))
                .await?;
            Ok(())
        })
    }
}
