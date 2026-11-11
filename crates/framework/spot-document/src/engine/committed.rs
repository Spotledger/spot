//! Running the `*_committed` steps after a transaction commits (ADR-51 phase 4).
//!
//! Delivery is behind two small traits so that the platform's later outbox-driven path
//! (WP-PLT-06) replaces [`InProcessDelivery`] without touching the engine: the engine hands the
//! committed steps of one top-level operation to a [`CommittedDelivery`], which decides when and
//! how often to run each through [`CommittedSteps`].

use spot_app_api::context::Invocation;
use spot_app_api::prelude::RecordId;
use spot_app_api::{AppError, AppResult, BoxFuture, Document, HookPoint, Message, Operation};
use spot_storage_contract::OutboxEvent;

/// One committed step to run: a `*_committed` hook point and the document it is about.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedStep {
    /// The hook point (`create_committed`, `save_committed`, ...).
    pub point: HookPoint,
    /// The document as written, or as it was before a deletion.
    pub doc: Document,
    /// The operation this is the committed phase of. The hook point alone does not say:
    /// `save_committed` runs after both create and update. Effects a step announces here belong
    /// to this operation's receipt.
    pub operation: Operation,
}

/// The committed steps of one top-level operation, in the order ADR-51 gives, with the
/// invocation they belong to.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedBatch {
    /// Who acted, and when; unchanged from the operation.
    pub invocation: Invocation,
    /// The steps, in order.
    pub steps: Vec<CommittedStep>,
}

/// What running one committed step came to: the messages it added, and the error that stopped it.
///
/// A failed step is recorded in [`Engine::committed_failures`](super::Engine::committed_failures)
/// as always; this also hands the error to a delivery path that must tell a business refusal
/// (never retried) from an infrastructure failure (retried, [`AppError::retryable`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepOutcome {
    /// The messages the step added, whether or not it failed.
    pub messages: Vec<Message>,
    /// The error that stopped the step, if it failed.
    pub failure: Option<AppError>,
}

/// What the engine offers a delivery path: running one committed hook point.
pub trait CommittedSteps: Send + Sync {
    /// Runs every step planned at `step.point` for the document's DocType, each with its own
    /// context. Writes the steps make are new top-level operations. A failure is recorded
    /// ([`Engine::committed_failures`](super::Engine::committed_failures)) and never rolls
    /// anything back; the messages the steps added are returned either way.
    fn run<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, Vec<Message>>;

    /// Like [`run`](Self::run), and also reports the error that stopped the step. The default
    /// runs the step and reports no error; the engine's own implementation reports it.
    fn run_checked<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, StepOutcome> {
        Box::pin(async move {
            StepOutcome {
                messages: self.run(invocation, step).await,
                failure: None,
            }
        })
    }
}

/// Where a durable delivery path finds the document a committed step is about, when the step
/// runs later than the commit (see [`super::durable`]).
pub trait StepSource: Send + Sync {
    /// The document as storage holds it now, read without a permission filter (the step is the
    /// platform's own); `None` if it no longer exists.
    fn current<'a>(
        &'a self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Option<Document>>>;
}

/// The outbox of the operation's own transaction, as a delivery path sees it in
/// [`CommittedDelivery::prepare`]. Object safe, because the transaction type is the backend's.
pub trait OutboxSink: Send {
    /// Captures `event`: published only if the transaction commits.
    fn enqueue<'a>(&'a mut self, event: &'a OutboxEvent) -> BoxFuture<'a, AppResult<()>>;
}

/// How committed steps are delivered after the commit.
pub trait CommittedDelivery: Send + Sync {
    /// Called once per attempt of a top-level operation, **inside its transaction** and after
    /// its last write, with the committed steps the operation collected (none: not called). What
    /// `outbox` captures commits with the document or rolls back with it. An error fails the
    /// operation and rolls everything back. The default does nothing.
    fn prepare<'a>(
        &'a self,
        _outbox: &'a mut dyn OutboxSink,
        _batch: &'a CommittedBatch,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Delivers the batch and returns the messages the steps produced, in order. Failures are
    /// the steps' own business (see [`CommittedSteps::run`]); delivery never fails the operation
    /// that committed.
    fn deliver<'a>(
        &'a self,
        steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<Message>>;
}

/// The v0 delivery: the steps run in the caller's task, in order, right after the commit.
///
/// A crash between the commit and the end of the steps loses them. The lifecycle event in the
/// outbox is the durable record; the platform's later path runs the steps from it.
#[derive(Clone, Copy, Debug, Default)]
pub struct InProcessDelivery;

impl CommittedDelivery for InProcessDelivery {
    fn deliver<'a>(
        &'a self,
        steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<Message>> {
        Box::pin(async move {
            let mut messages = Vec::new();
            for step in batch.steps {
                messages.extend(steps.run(&batch.invocation, step).await);
            }
            messages
        })
    }
}
