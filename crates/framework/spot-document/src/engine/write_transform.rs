//! The write-transform seam (WP-SEC-10.T7; ADR-070 section 12): code that replaces the value of a
//! field in the document the engine is about to store, inside the write's own transaction.
//!
//! The first user is the sealed-fields module: a protected field holds a box to the hub's key (or
//! an encrypted-group form), never the plaintext, so the engine must be able to turn the value a
//! caller wrote into the value that is stored before anything else sees it.
//!
//! # Where it runs
//!
//! ```text
//! authorise → before_* steps → … → validate_* → check (types, links)
//!   → [ WriteTransform.transform ]
//!   → [ participants.before_write → storage write + lifecycle event → participants.after_write ]
//! ```
//!
//! After authorisation and validation (so a refused or invalid write never reaches it, and the
//! checks see the value the caller wrote), before the participants (so a plaintext guard sees the
//! stored form) and before persistence. Because it changes the document the write stage holds, the
//! lifecycle event, the `doc_state` capture, the search index and the audit image that follow are
//! all made from the transformed document: no plaintext reaches them. A transform that fails
//! aborts the write and the transaction rolls back; its error is returned unchanged, so a
//! transform must never put a value in one. A transform that panics is caught and answered with
//! `document.internal_error`. A delete is not transformed (there is nothing to store).
//!
//! The default is [`IdentityTransform`]: with nothing installed the engine behaves exactly as
//! before.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use spot_app_api::context::Invocation;
use spot_app_api::{AppError, AppResult, Document, ErrorClass, Operation};
use spot_storage_contract::StorageBackend;

use super::Engine;

/// What a transform is told about the write (a read-only view; the document itself is the
/// separate `&mut` argument).
#[derive(Clone, Copy, Debug)]
pub struct WriteView<'a> {
    /// The DocType of the document.
    pub doctype: &'a str,
    /// The operation that causes the write.
    pub operation: Operation,
    /// Who is acting and when (the invocation's own time: a transform reads no other clock for
    /// anything it stores).
    pub invocation: &'a Invocation,
}

/// Rewrites the document of a write just before it is stored.
///
/// Implementations are synchronous and must not block: the transform runs inside the write's
/// transaction. They must be idempotent (a document read back and saved again carries values that
/// are already in their stored form) and must not put a field value in an error.
pub trait WriteTransform: Send + Sync + 'static {
    /// Replaces values in `doc`. An error aborts the write.
    ///
    /// # Errors
    ///
    /// Whatever the transform refuses, as an [`AppError`] that names no value.
    fn transform(&self, write: &WriteView<'_>, doc: &mut Document) -> AppResult<()>;
}

/// The transform that changes nothing: the engine's default.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityTransform;

impl WriteTransform for IdentityTransform {
    fn transform(&self, _write: &WriteView<'_>, _doc: &mut Document) -> AppResult<()> {
        Ok(())
    }
}

impl<B: StorageBackend> Engine<B> {
    /// The engine with `transform` run on every document it writes (see the module docs for where).
    #[must_use]
    pub fn with_write_transform(self, transform: Arc<dyn WriteTransform>) -> Self {
        self.set_write_transform(transform);
        self
    }

    /// Replaces the write transform (a site's protection settings changed), like
    /// [`Engine::set_participants`]: writes already in flight keep the one they began with.
    pub fn set_write_transform(&self, transform: Arc<dyn WriteTransform>) {
        let mut guard = self
            .write_transform
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = transform;
    }

    /// The transform now installed, as one write sees it.
    pub(crate) fn write_transform(&self) -> Arc<dyn WriteTransform> {
        Arc::clone(
            &self
                .write_transform
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The write stage's one call: transforms `doc` for `operation` unless it is a delete.
    ///
    /// # Errors
    ///
    /// The transform's error, or `document.internal_error` when it panicked.
    pub(crate) fn transform_for_write(
        &self,
        inv: &Invocation,
        operation: Operation,
        doc: &mut Document,
    ) -> AppResult<()> {
        if operation == Operation::Delete {
            return Ok(());
        }
        let transform = self.write_transform();
        let doctype = doc.doctype().as_str().to_owned();
        let view = WriteView {
            doctype: &doctype,
            operation,
            invocation: inv,
        };
        match catch_unwind(AssertUnwindSafe(|| transform.transform(&view, doc))) {
            Ok(result) => result,
            Err(_) => Err(AppError::new(
                ErrorClass::Internal,
                "document.internal_error",
                "A write transform failed.",
            )),
        }
    }
}
