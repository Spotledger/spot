//! The lifecycle step that writes the audit event, and the outbox enqueue it performs.
//!
//! [`AuditStep`] attaches to a DocType's `after_*` hook points: the write has happened, the step
//! is inside the business transaction, and phase 3 may enqueue. It builds a [`DocAuditEvent`]
//! from the document's before and after images and hands it to an [`AuditWriter`], which writes
//! it to the outbox **in the same transaction**. If the write fails the operation fails closed:
//! nothing is changed without its audit event.
//!
//! # The contract gap this crate works around
//!
//! A controller step cannot reach the transaction directly, and `HookContext::publish` refuses
//! the reserved `sys_` prefix (ADR-056, ADR-061) — the prefix every audit-class topic uses. The
//! engine therefore supplies an [`AuditWriter`] bound to the current transaction (a later
//! integration task), and [`enqueue_audit`] is the exact enqueue it performs; this crate's tests
//! drive it against a real transactional outbox. See `## Open questions` in the task report.

use std::sync::Arc;

use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, Version};
use spot_app_api::{
    AppResult, BoxFuture, Controller, Document, HookContext, Invocation, StepDecl, StepName,
};
use spot_core::Clock;
use spot_events_contract::Topic;
use spot_events_contract::payload::{Actor as EventActor, EventMeta, PayloadDocument};
use spot_storage_contract::feed::Outbox;
use spot_storage_contract::{NodeRef, OutboxEvent};

use crate::error::DocAuditError;
use crate::event::{
    ActorRef, AuditSource, DocAuditEvent, DocOperation, DocumentImage, EventSizePolicy, build_event,
};
use crate::masker::FieldMasker;

/// The audit-class topic the document audit events are written under. Reserved `sys_` prefix
/// (ADR-061 section 2); registering it in the events contract is the lead's task (see the
/// report's open questions).
pub const AUDIT_TOPIC: &str = "sys_doc_audit";

/// The payload schema version of an audit event. Version 2 adds the optional `method`,
/// `correction_half`, `via`, `node`, `outcome`, `error_class` and `error_code` (WP-SEC-06.T3);
/// a version 1 event still decodes (the new fields default to absent) and the chain verifier is
/// unaffected, because it hashes the stored bytes.
pub const SCHEMA_VERSION: u32 = 2;

/// DocTypes that may be marked `audit = off`: framework-internal DocTypes that are themselves
/// audited elsewhere. A business DocType must never appear here; the crate's tests fail if the
/// list changes at all, so an addition is a deliberate review decision.
pub const AUDIT_OFF_DOCTYPES: &[&str] = &[
    "audit_chain",
    "audit_checkpoint",
    "doc_audit_event",
    "sys_telemetry",
];

/// Whether a DocType is on the audit-off list.
#[must_use]
pub fn is_audit_off(doctype: &str) -> bool {
    AUDIT_OFF_DOCTYPES.contains(&doctype)
}

/// Where the audit step writes the event: the engine binds it to the business transaction.
pub trait AuditWriter: Send + Sync {
    /// Writes `event` to the outbox in the caller's transaction.
    ///
    /// # Errors
    /// [`DocAuditError::OutboxWrite`] when the outbox refuses, so the operation fails closed.
    fn write(&self, event: &DocAuditEvent) -> Result<(), DocAuditError>;
}

/// A writer that fails every write, for a fail-closed test.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailingWriter;

impl AuditWriter for FailingWriter {
    fn write(&self, _event: &DocAuditEvent) -> Result<(), DocAuditError> {
        Err(DocAuditError::OutboxWrite(
            "the outbox is unavailable".to_owned(),
        ))
    }
}

/// The before image of a document, when the engine has one. The document handle exposes what
/// changed but not the loaded snapshot, so the engine supplies it.
pub trait BeforeImages: Send + Sync {
    /// The image the document was loaded with, if the engine can produce one.
    fn before(&self, doctype: &str, id: &str) -> Option<DocumentImage>;
}

/// No before images: the diff then holds only the after side of each changed field.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBeforeImages;

impl BeforeImages for NoBeforeImages {
    fn before(&self, _doctype: &str, _id: &str) -> Option<DocumentImage> {
        None
    }
}

/// A before-image source that returns a fixed image for one document.
#[derive(Clone, Debug, Default)]
pub struct FixedBeforeImages {
    images: Vec<DocumentImage>,
}

impl FixedBeforeImages {
    /// A source over `images`.
    #[must_use]
    pub fn new(images: Vec<DocumentImage>) -> Self {
        Self { images }
    }
}

impl BeforeImages for FixedBeforeImages {
    fn before(&self, doctype: &str, id: &str) -> Option<DocumentImage> {
        self.images
            .iter()
            .find(|i| i.doctype == doctype && i.id == id)
            .cloned()
    }
}

/// The document audit lifecycle step.
pub struct AuditStep {
    doctype: String,
    writer: Arc<dyn AuditWriter>,
    masker: Arc<dyn FieldMasker>,
    before: Arc<dyn BeforeImages>,
    policy: EventSizePolicy,
    source: AuditSource,
    clock: Arc<dyn Clock>,
}

impl AuditStep {
    /// A step for `doctype`, writing through `writer` and masking through `masker`.
    #[must_use]
    pub fn new(
        doctype: &str,
        writer: Arc<dyn AuditWriter>,
        masker: Arc<dyn FieldMasker>,
        before: Arc<dyn BeforeImages>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            doctype: doctype.to_owned(),
            writer,
            masker,
            before,
            policy: EventSizePolicy::default(),
            source: AuditSource::Api,
            clock,
        }
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

    /// Builds the event for one hook point and document; `None` when the point is not an audited
    /// operation or the DocType is on the audit-off list.
    ///
    /// # Errors
    /// A masker, encoding or audit-off error.
    pub fn build(
        &self,
        point: &spot_app_api::HookPoint,
        doc: &Document,
        inv: &Invocation,
    ) -> Result<Option<DocAuditEvent>, DocAuditError> {
        let Some(operation) = operation_of_point(point) else {
            return Ok(None);
        };
        if is_audit_off(self.doctype.as_str()) {
            return Ok(None);
        }
        let after = DocumentImage::of(doc);
        let before = self.before.before(&after.doctype, &after.id);
        let actor = ActorRef::of(&inv.actor);
        let event = build_event(
            &*self.masker,
            inv.site.as_str(),
            operation,
            &after,
            before.as_ref(),
            actor,
            micros(inv.now),
            inv.command_id.as_ref().map(|c| c.as_str().to_owned()),
            self.source,
            &self.policy,
        )?;
        Ok(Some(event))
    }

    /// Builds and writes the event, the whole of what [`Controller::run_step`] does.
    ///
    /// # Errors
    /// [`DocAuditError::OutboxWrite`] when the writer fails, so the operation fails closed.
    pub fn build_and_write(
        &self,
        point: &spot_app_api::HookPoint,
        doc: &Document,
        inv: &Invocation,
    ) -> Result<(), DocAuditError> {
        if let Some(event) = self.build(point, doc, inv)? {
            self.writer.write(&event)?;
        }
        Ok(())
    }

    /// The DocType this step audits.
    #[must_use]
    pub fn audits(&self) -> &str {
        &self.doctype
    }

    /// The clock the step reads.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

impl Controller for AuditStep {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        use spot_app_api::{HookPoint, Operation, Phase, Target};
        let points = [
            (
                "doc_audit_create",
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            ),
            (
                "doc_audit_update",
                HookPoint::of(Phase::After, Target::Op(Operation::Update)),
            ),
            (
                "doc_audit_update_posted",
                HookPoint::of(Phase::After, Target::Op(Operation::UpdatePosted)),
            ),
            (
                "doc_audit_delete",
                HookPoint::of(Phase::After, Target::Op(Operation::Delete)),
            ),
            (
                "doc_audit_post",
                HookPoint::of(Phase::After, Target::Op(Operation::Post)),
            ),
            (
                "doc_audit_cancel",
                HookPoint::of(Phase::After, Target::Op(Operation::Cancel)),
            ),
            (
                "doc_audit_rename",
                HookPoint::of(Phase::After, Target::Op(Operation::Rename)),
            ),
        ];
        let mut steps = Vec::new();
        for (name, point) in points {
            let Ok(step_name) = StepName::new(&format!("platform:{name}")) else {
                continue;
            };
            steps.push(StepDecl {
                name: step_name,
                point,
                // Platform steps use multiples of 100 (ADR-51); the audit runs late, after the
                // write, and does not change the document.
                priority: 900,
                budget: std::time::Duration::from_secs(5),
            });
        }
        steps
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some(operation) = operation_of_step(step.as_str()) else {
                return Ok(());
            };
            if operation_of_point(ctx.point()) != Some(operation) {
                return Ok(());
            }
            let inv = ctx.invocation().clone();
            let doc = ctx.doc().clone();
            self.build_and_write(ctx.point(), &doc, &inv)
                .map_err(|e| e.to_app_error())
        })
    }
}

/// The operation an `after_*` hook point audits; `None` for any other point.
#[must_use]
pub fn operation_of_point(point: &spot_app_api::HookPoint) -> Option<DocOperation> {
    use spot_app_api::{HookPoint, Operation, Phase, Target};
    match point {
        HookPoint::Lifecycle {
            phase: Phase::After,
            target: Target::Op(op),
        } => Some(match op {
            Operation::Create => DocOperation::Create,
            Operation::Update => DocOperation::Update,
            Operation::Delete => DocOperation::Delete,
            Operation::Post => DocOperation::Post,
            Operation::Cancel => DocOperation::Cancel,
            Operation::Rename => DocOperation::Rename,
            Operation::UpdatePosted => DocOperation::Update,
        }),
        _ => None,
    }
}

/// Whether `name` is one of the step names [`AuditStep`] declares: `platform:doc_audit_<op>`.
/// The install-time double-registration check recognises an audit step by this, so it needs no
/// change outside this crate.
#[must_use]
pub fn is_audit_step_name(name: &str) -> bool {
    name.strip_prefix("platform:")
        .is_some_and(|op| operation_of_step(op).is_some())
}

/// The operation a `platform:doc_audit_<op>` step name audits.
#[must_use]
pub fn operation_of_step(step: &str) -> Option<DocOperation> {
    match step.rsplit(':').next()? {
        "doc_audit_create" => Some(DocOperation::Create),
        "doc_audit_update" | "doc_audit_update_posted" => Some(DocOperation::Update),
        "doc_audit_delete" => Some(DocOperation::Delete),
        "doc_audit_post" => Some(DocOperation::Post),
        "doc_audit_cancel" => Some(DocOperation::Cancel),
        "doc_audit_rename" => Some(DocOperation::Rename),
        _ => None,
    }
}

/// Unix microseconds of a timestamp.
#[must_use]
pub fn micros(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1000).unwrap_or(0)
}

/// The timestamp of Unix microseconds.
#[must_use]
pub fn from_micros(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Writes `event` to `tx`'s outbox as a `sys_doc_audit` event, in the same transaction. The
/// engine's transaction-bound [`AuditWriter`] calls exactly this.
///
/// # Errors
/// [`DocAuditError::OutboxWrite`] when the outbox refuses; [`DocAuditError::Encoding`] when the
/// event or its subject cannot be encoded.
pub async fn enqueue_audit<O: Outbox + ?Sized>(
    tx: &mut O,
    event: &DocAuditEvent,
) -> Result<(), DocAuditError> {
    let outbox_event = outbox_event_of(event)?;
    tx.enqueue(&outbox_event)
        .await
        .map_err(|e| DocAuditError::OutboxWrite(e.explanation))
}

/// The outbox event an audit event becomes.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the topic, subject or payload is malformed.
pub fn outbox_event_of(event: &DocAuditEvent) -> Result<OutboxEvent, DocAuditError> {
    let topic = Topic::any(AUDIT_TOPIC).map_err(|e| DocAuditError::Encoding(e.to_string()))?;
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: from_micros(event.at),
        actor: event_actor(&event.actor)?,
        command_id: event
            .command_id
            .as_deref()
            .and_then(|c| CommandId::new(c).ok()),
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let data = serde_json::to_value(event).map_err(|e| DocAuditError::Encoding(e.to_string()))?;
    let doc = PayloadDocument { meta, data };
    let subject = NodeRef {
        entity: Name::new(&event.doctype).map_err(|e| DocAuditError::Encoding(e.to_string()))?,
        id: record_id_of(&event.id)?,
    };
    doc.to_outbox_event(
        id,
        &topic,
        subject,
        Some(Version(event.version)),
        SCHEMA_VERSION,
    )
    .map_err(|e| DocAuditError::Encoding(e.to_string()))
}

/// The events-contract actor of an audit actor.
fn event_actor(actor: &ActorRef) -> Result<EventActor, DocAuditError> {
    if let Some(id) = actor.actor.strip_prefix("user:") {
        return Ok(EventActor::User {
            id: record_id_of(id)?,
            via: actor
                .on_behalf_of
                .as_deref()
                .and_then(|v| v.strip_prefix("user:"))
                .map(record_id_of)
                .transpose()?,
        });
    }
    let component = actor
        .component
        .as_deref()
        .and_then(|c| Name::new(c).ok())
        .or_else(|| Name::new("platform").ok())
        .ok_or_else(|| DocAuditError::Encoding("the system component is not a name".to_owned()))?;
    Ok(EventActor::System { component })
}

/// Parses a hyphenated UUID into a storage record id, without a UUID dependency.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the text is not a UUIDv7.
pub fn record_id_of(text: &str) -> Result<RecordId, DocAuditError> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(DocAuditError::Encoding(format!(
            "`{text}` is not a record id"
        )));
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let part = hex
            .get(i * 2..i * 2 + 2)
            .ok_or_else(|| DocAuditError::Encoding(format!("`{text}` is not a record id")))?;
        *byte = u8::from_str_radix(part, 16)
            .map_err(|_| DocAuditError::Encoding(format!("`{text}` is not a record id")))?;
    }
    RecordId::from_bytes(bytes).map_err(|e| DocAuditError::Encoding(e.explanation))
}
