//! The audit event written inside the document's own transaction (WP-SEC-06.T2).
//!
//! [`AuditStep`](crate::AuditStep) is a controller step: it cannot reach the transaction, and
//! `HookContext::publish` refuses the reserved `sys_` prefix. The document engine's
//! [`TxParticipant`] seam can: it lends the open transaction right after each storage write, so
//! [`AuditParticipant`] builds the masked [`DocAuditEvent`] from the write and enqueues it with
//! [`enqueue_audit`] on **that same transaction handle**.
//!
//! # What this guarantees
//!
//! - **Atomic.** The audit row and the document commit or roll back together: no change without
//!   its audit event, and no audit event for a change that did not happen.
//! - **Fails closed.** If the event cannot be built (the masker does not know the DocType, the
//!   event cannot be encoded) or the outbox refuses it, the participant returns an error and the
//!   engine rolls the whole operation back. A write the audit cannot record does not happen.
//! - **One event per stored version.** Every write path passes through the engine's single write
//!   stage, so create, update, update of a posted document, post, cancel, delete, rename, both
//!   halves of a correction (the cancel, then the amended draft, in that order) and every write a
//!   whitelisted method makes each produce exactly one event.
//! - **No secret in the row.** The diff is built from the write's before and after images and
//!   masked through the [`FieldMasker`] (ADR-070 section 12 rule 9) before it is encoded; a long
//!   text or an attachment is a length and a digest.
//! - **Off the hot path.** The participant only enqueues. Chaining happens afterwards in the
//!   lane appender.
//! - **No recursion.** A participant is given the transaction and the write, never the engine,
//!   so it cannot start an operation; and a DocType on [`AUDIT_OFF_DOCTYPES`] (the audit's own
//!   DocTypes) gets no event.
//!
//! # What the seam tells it (WP-SEC-06.T3)
//!
//! The seam reports the [`Operation`] and, since WP-DOC-04.T5, the [`WriteCause`] and the
//! deployment node. The participant records them:
//!
//! - a whitelisted method's write is a `DocOperation::Method` with the declared name in the
//!   event's `method` field. The name is bounded (1 to 64 bytes of `[a-z0-9_]`); one that is not
//!   is dropped, never recorded, and the event then carries the base operation with
//!   `via = "method"`;
//! - the two halves of a correction are `DocOperation::Correct` with `correction_half` `cancel`
//!   then `amend`;
//! - a write a lifecycle step made records the operation of the step's own write with
//!   `via = "step"`;
//! - the seam's `node` is recorded when set.
//!
//! A refused operation leaves no write, so it leaves no participant call: the
//! [`AuditDenialSink`](crate::AuditDenialSink) records it.
//!
//! # Do not register both
//!
//! A host registers this participant **or** the [`AuditStep`](crate::AuditStep) for a DocType,
//! never both, or every write is audited twice. [`checked_participants`](crate::checked_participants)
//! refuses the combination when the participant is told which DocTypes it audits
//! ([`AuditParticipant::with_doctypes`]).

use std::sync::Arc;

use spot_app_api::{AppResult, BoxFuture, Document, Operation};
use spot_document::engine::{TxParticipant, WriteCause, WriteRecord};
use spot_storage_contract::Transaction;
use spot_storage_contract::feed::Outbox;

use crate::error::DocAuditError;
use crate::event::{
    ActorRef, AuditSource, DocAuditEvent, DocOperation, DocumentImage, EventExtra, EventSizePolicy,
    build_event_with, is_recordable_method,
};
use crate::masker::FieldMasker;
use crate::step::{enqueue_audit, is_audit_off, micros};

/// The transaction participant that writes the `sys_doc_audit` event of every document write.
pub struct AuditParticipant {
    masker: Arc<dyn FieldMasker>,
    policy: EventSizePolicy,
    source: AuditSource,
    doctypes: Vec<String>,
}

impl AuditParticipant {
    /// A participant masking through `masker`, with the default size policy, recording changes
    /// as coming from the API.
    #[must_use]
    pub fn new(masker: Arc<dyn FieldMasker>) -> Self {
        Self {
            masker,
            policy: EventSizePolicy::default(),
            source: AuditSource::Api,
            doctypes: Vec::new(),
        }
    }

    /// Names the DocTypes this participant audits, so
    /// [`checked_participants`](crate::checked_participants) can look for an audit step on them.
    /// The controller registry cannot list its DocTypes, so the host says which to check.
    #[must_use]
    pub fn with_doctypes<I, S>(mut self, doctypes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.doctypes = doctypes.into_iter().map(Into::into).collect();
        self
    }

    /// The DocTypes named with [`with_doctypes`](Self::with_doctypes).
    #[must_use]
    pub fn doctypes(&self) -> &[String] {
        &self.doctypes
    }

    /// Sets the size policy.
    #[must_use]
    pub fn with_policy(mut self, policy: EventSizePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets where changes are recorded as coming from.
    #[must_use]
    pub fn with_source(mut self, source: AuditSource) -> Self {
        self.source = source;
        self
    }

    /// The event for one write; `None` for a DocType on the audit-off list.
    ///
    /// # Errors
    /// A masker or encoding error, or [`DocAuditError::Encoding`] when the write names no
    /// document at all. Every error fails the operation closed.
    pub fn build(&self, write: &WriteRecord<'_>) -> Result<Option<DocAuditEvent>, DocAuditError> {
        if is_audit_off(write.doctype) {
            return Ok(None);
        }
        let (before, after) = images(write)?;
        let (operation, mut extra) = operation_for(write.operation, write.cause);
        extra.node = write.node.map(str::to_owned);
        let event = build_event_with(
            &*self.masker,
            write.invocation.site.as_str(),
            operation,
            &after,
            before.as_ref(),
            ActorRef::of(&write.invocation.actor),
            micros(write.invocation.now),
            write
                .invocation
                .command_id
                .as_ref()
                .map(|c| c.as_str().to_owned()),
            self.source,
            &self.policy,
            extra,
        )?;
        Ok(Some(event))
    }

    /// Builds the event for `write` and enqueues it on `outbox`, the transaction's own outbox.
    /// This is exactly what [`TxParticipant::after_write`] does, with the outbox named so a test
    /// can hand it one that fails.
    ///
    /// # Errors
    /// The app error of any [`DocAuditError`]: the operation then fails closed.
    pub async fn write_to<O: Outbox>(
        &self,
        outbox: &mut O,
        write: &WriteRecord<'_>,
    ) -> AppResult<()> {
        let Some(event) = self.build(write).map_err(|e| e.to_app_error())? else {
            return Ok(());
        };
        enqueue_audit(outbox, &event)
            .await
            .map_err(|e| e.to_app_error())
    }
}

impl<Tx: Transaction> TxParticipant<Tx> for AuditParticipant {
    fn after_write<'a>(
        &'a self,
        tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { self.write_to(tx, write).await })
    }
}

/// The audit operation of a lifecycle operation.
#[must_use]
pub fn operation_of(operation: Operation) -> DocOperation {
    match operation {
        Operation::Create => DocOperation::Create,
        Operation::Update | Operation::UpdatePosted => DocOperation::Update,
        Operation::Delete => DocOperation::Delete,
        Operation::Post => DocOperation::Post,
        Operation::Cancel => DocOperation::Cancel,
        Operation::Rename => DocOperation::Rename,
    }
}

/// The audit operation of a write and the event fields its cause adds (never the node).
#[must_use]
pub fn operation_for(operation: Operation, cause: WriteCause<'_>) -> (DocOperation, EventExtra) {
    let mut extra = EventExtra::default();
    let op = match cause {
        WriteCause::Method { name } if is_recordable_method(name) => {
            extra.method = Some(name.to_owned());
            DocOperation::Method(name.to_owned())
        }
        WriteCause::Method { .. } => {
            // The name is not trusted: keep the base operation and say a method made the write.
            extra.via = Some("method".to_owned());
            operation_of(operation)
        }
        WriteCause::CorrectCancel => {
            extra.correction_half = Some("cancel".to_owned());
            DocOperation::Correct
        }
        WriteCause::CorrectAmend => {
            extra.correction_half = Some("amend".to_owned());
            DocOperation::Correct
        }
        WriteCause::Step => {
            extra.via = Some("step".to_owned());
            operation_of(operation)
        }
        _ => operation_of(operation),
    };
    (op, extra)
}

/// The before and after images of a write. A delete's after image is empty (so the diff shows
/// every field removed) and carries the last stored version.
fn images(
    write: &WriteRecord<'_>,
) -> Result<(Option<DocumentImage>, DocumentImage), DocAuditError> {
    let before = write.old.map(DocumentImage::of);
    let after = match (write.new, write.old) {
        (Some(new), _) => {
            let mut image = DocumentImage::of(new);
            // The version storage assigned, not whatever the handle held.
            image.version = write.version.or_else(|| new.version()).map_or(0, |v| v.0);
            image
        }
        (None, Some(old)) => deleted_image(old),
        (None, None) => {
            return Err(DocAuditError::Encoding(
                "the write names no document".to_owned(),
            ));
        }
    };
    Ok((before, after))
}

fn deleted_image(old: &Document) -> DocumentImage {
    DocumentImage::new(
        old.doctype().as_str(),
        &old.id().to_string(),
        old.version().map_or(0, |v| v.0),
    )
}
