//! Transaction participants: code that runs inside a document's own transaction around its
//! storage write (WP-DOC-04.T3; ADR-51).
//!
//! The engine's `after_*` steps cannot reach the transaction, and several platform crates need
//! to: reconcile edges, write an audit event, check partition ownership. A [`TxParticipant`] is
//! the one general seam for them. It is handed the open transaction and a read-only view of the
//! write, and it knows nothing about edges, audit or partitions: what it does is its own.
//!
//! # Where they run
//!
//! For every document write, whatever caused it (create, update, post, cancel, delete, rename,
//! update of a posted document, both halves of a correction, a write a whitelisted method or a
//! step makes through the repository: all of them pass through the engine's single write stage):
//!
//! ```text
//! authorise → before_* steps → … → validate_* → check
//!   → [ participants.before_write → storage write + lifecycle event → participants.after_write ]
//!   → after_* steps → [commit]
//! ```
//!
//! `before_write` runs after authorisation and the phase-1 steps, and before the storage write;
//! `after_write` runs immediately after the write (and its lifecycle event), before any `after_*`
//! step. Participants run in registration order, one at a time. An error from either call fails
//! the operation, and the transaction rolls back: the document, the outbox events and anything
//! the participants wrote through the transaction go together. In a correction the cancel of the
//! original and the creation of the amended draft each fire, in that order, and a failure in the
//! second rolls back the first. A write made by a step runs inside a savepoint that is rolled
//! back if it fails, so a failing participant leaves nothing of that write behind either.
//!
//! A participant is given the transaction and the write, never the engine, so it cannot start a
//! nested operation: no re-entrancy, hence no recursion. Reads (`load`, `query`) and `field_changed`
//! call nothing.
//!
//! # Not run here
//!
//! Replica-apply and import paths (WP-EDG-06) write documents without going through the engine,
//! so participants are **not** run for them. That is deliberate: those paths apply a record whose
//! effects (edges, audit images, ownership) were decided on the node that made it, and replaying
//! the local participants would apply them twice or under the wrong owner. They carry their own
//! checks.
//!
//! # Failures and panics
//!
//! A participant's [`AppError`] is returned unchanged; participants map their own storage errors
//! (the engine adds none of its own here), and the engine never wraps one in a storage detail.
//! A participant that panics is caught at the engine boundary and answered with
//! `document.internal_error`; the operation fails and the transaction rolls back.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use spot_app_api::context::Invocation;
use spot_app_api::prelude::{RecordId, Version};
use spot_app_api::{AppError, AppResult, BoxFuture, Document, ErrorClass, Operation};
use spot_storage_contract::Transaction;

/// Why a document is written, beyond the [`Operation`] (WP-DOC-04.T5).
///
/// A whitelisted method's write is just an `Update` and the two halves of a correction are a
/// `Cancel` and a `Create`; this says which of them it was, for an audit that must tell them
/// apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteCause<'a> {
    /// A whitelisted method wrote the document (its repository call). `name` is the method's
    /// name as declared.
    Method {
        /// The method's declared name.
        name: &'a str,
    },
    /// The cancel of the original in a correction.
    CorrectCancel,
    /// The creation of the amended draft in a correction.
    CorrectAmend,
    /// A lifecycle step wrote the document through the repository.
    Step,
    /// Any other write: the caller asked for the operation itself.
    Direct,
}

/// The deployment node (ADR-062) an engine writes for: a plain string, so this crate needs no
/// dependency on the sync protocol. Set with [`Engine::with_node`](super::Engine::with_node).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// A node id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The owned form of a [`WriteCause`], carried in the transaction's working state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Cause {
    #[default]
    Direct,
    Method(String),
    CorrectCancel,
    CorrectAmend,
    Step,
}

impl Cause {
    pub(crate) fn view(&self) -> WriteCause<'_> {
        match self {
            Cause::Direct => WriteCause::Direct,
            Cause::Method(name) => WriteCause::Method { name },
            Cause::CorrectCancel => WriteCause::CorrectCancel,
            Cause::CorrectAmend => WriteCause::CorrectAmend,
            Cause::Step => WriteCause::Step,
        }
    }
}

/// A refused operation, as the [`DenialSink`] is told (WP-DOC-04.T5).
///
/// It names who tried what and the error's class and code. The error it holds has an **empty
/// message and no details or field errors**: those can name field values, and a denial record is
/// itself sensitive.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Denial<'a> {
    /// The DocType the operation was on.
    pub doctype: &'a str,
    /// The document, when the request named one (a create names the id it was given).
    pub id: Option<RecordId>,
    /// The operation that was refused. A refused correction is the `Cancel` or the `Create`
    /// half, whichever was running.
    pub operation: Operation,
    /// Why the write would have been made (see [`WriteCause`]).
    pub cause: WriteCause<'a>,
    /// Who acted, for which site, under which command.
    pub invocation: &'a Invocation,
    /// The class and code of the refusal; the message and details are removed.
    pub error: &'a AppError,
}

/// Told about every refused operation, after its transaction has rolled back.
///
/// Installed with [`Engine::set_denial_sink`](super::Engine::set_denial_sink). The sink runs on
/// its own: it may open its own transaction, and nothing it does can change the operation's
/// error. A panic or a call longer than the engine's denial timeout is logged
/// (`document.denial_sink_failed`) and dropped. The bound is on awaiting: a sink that blocks the
/// thread synchronously without ever awaiting is not cut off by the timeout, so a sink must not
/// block (do its slow work on its own task).
pub trait DenialSink: Send + Sync {
    /// One operation was refused.
    fn denied(&self, denial: &Denial<'_>) -> BoxFuture<'_, ()>;
}

/// What is about to be written: a read-only view for [`TxParticipant::before_write`].
#[derive(Clone, Copy, Debug)]
pub struct WriteIntent<'a> {
    /// The DocType being written.
    pub doctype: &'a str,
    /// The operation causing the write. A write by a whitelisted method or a step is an
    /// `Update` (or whichever operation its lifecycle runs); [`cause`](Self::cause) says why.
    pub operation: Operation,
    /// Why the write is made: a whitelisted method, a half of a correction, a step, or a direct
    /// call (WP-DOC-04.T5).
    pub cause: WriteCause<'a>,
    /// The deployment node (ADR-062) that made the write; `None` in a single-node site.
    pub node: Option<&'a str>,
    /// The document as storage held it before this operation; `None` on create.
    pub old: Option<&'a Document>,
    /// The document about to be written, after the `before_*` steps and the checks; `None`
    /// on delete.
    pub new: Option<&'a Document>,
    /// Who is acting, for which site, under which command, at which business time.
    pub invocation: &'a Invocation,
}

/// What was written: a read-only view for [`TxParticipant::after_write`].
#[derive(Clone, Copy, Debug)]
pub struct WriteRecord<'a> {
    /// The DocType written.
    pub doctype: &'a str,
    /// The operation that caused the write.
    pub operation: Operation,
    /// Why the write was made (see [`WriteIntent::cause`]).
    pub cause: WriteCause<'a>,
    /// The deployment node (ADR-062) that made the write; `None` in a single-node site.
    pub node: Option<&'a str>,
    /// The document as storage held it before this operation; `None` on create.
    pub old: Option<&'a Document>,
    /// The document as written, with its new version and state; `None` on delete.
    pub new: Option<&'a Document>,
    /// The version storage assigned; `None` on delete.
    pub version: Option<Version>,
    /// Who is acting, for which site, under which command, at which business time.
    pub invocation: &'a Invocation,
}

/// Code that runs in the document's transaction around each storage write.
///
/// Generic over the backend's transaction because [`Transaction`] is not object-safe. The same
/// transaction handle the engine writes through is lent for the call, so whatever a participant
/// writes commits or rolls back with the document. Both methods default to doing nothing.
pub trait TxParticipant<Tx: Transaction>: Send + Sync {
    /// Called after authorisation and the phase-1 steps, before the storage write. An error
    /// refuses the write and the operation rolls back; nothing is stored.
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        _write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Called right after the storage write, before the `after_*` steps. An error rolls back
    /// the whole operation, the document included.
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        _write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// The registered participants, in order, as one write sees them.
pub(crate) type Participants<Tx> = Arc<Vec<Arc<dyn TxParticipant<Tx>>>>;

/// Runs every `before_write`, in order; the first error stops the rest.
pub(crate) async fn run_before<Tx: Transaction>(
    participants: &Participants<Tx>,
    tx: &mut Tx,
    intent: &WriteIntent<'_>,
) -> AppResult<()> {
    for participant in participants.iter() {
        guarded(|| participant.before_write(tx, intent)).await?;
    }
    Ok(())
}

/// Runs every `after_write`, in order; the first error stops the rest.
pub(crate) async fn run_after<Tx: Transaction>(
    participants: &Participants<Tx>,
    tx: &mut Tx,
    record: &WriteRecord<'_>,
) -> AppResult<()> {
    for participant in participants.iter() {
        guarded(|| participant.after_write(tx, record)).await?;
    }
    Ok(())
}

/// Awaits a participant's future, turning a panic into a stable error.
/// A panic while the future is built (before its first poll) is caught as well.
async fn guarded<'a>(make: impl FnOnce() -> BoxFuture<'a, AppResult<()>>) -> AppResult<()> {
    match catch_unwind(AssertUnwindSafe(make)) {
        Ok(fut) => CatchUnwind { fut }.await,
        Err(_) => Err(panicked()),
    }
}

fn panicked() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "document.internal_error",
        "A transaction participant failed unexpectedly; the operation was rolled back.",
    )
}

/// A future whose panic on poll is caught. The caller drops the transaction's work on the error
/// it returns, so nothing observes state a panic left half-done.
struct CatchUnwind<'a> {
    fut: BoxFuture<'a, AppResult<()>>,
}

impl Future for CatchUnwind<'_> {
    type Output = AppResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.fut.as_mut().poll(cx))) {
            Ok(poll) => poll,
            Err(_) => Poll::Ready(Err(panicked())),
        }
    }
}

/// Tells the sink about a refusal, bounded by `limit`. Its panic or overrun is logged and
/// dropped: the sink can never change the operation's error, and a slow one cannot hang it.
pub(crate) async fn report_denial(
    sink: &dyn DenialSink,
    denial: &Denial<'_>,
    limit: std::time::Duration,
) {
    // `true` when the sink ran to its end.
    let call = async {
        let Ok(mut fut) = catch_unwind(AssertUnwindSafe(|| sink.denied(denial))) else {
            return false;
        };
        std::future::poll_fn(
            |cx| match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(())) => Poll::Ready(true),
                Err(_) => Poll::Ready(false),
            },
        )
        .await
    };
    // Only the fact of the failure is logged: never the denial's content.
    match tokio::time::timeout(limit, call).await {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            tracing::warn!(
                code = "document.denial_sink_failed",
                "the denial sink panicked or took too long; the refusal stands"
            );
        }
    }
}
