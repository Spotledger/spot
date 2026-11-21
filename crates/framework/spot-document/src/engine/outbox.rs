//! The events the engine writes to the outbox in the business transaction (ADR-11, ADR-056):
//! the lifecycle event of every operation, and what steps enqueue in phase 3.

use serde_json::{Value as Json, json};
use spot_app_api::context::{Invocation, JobRequest};
use spot_app_api::error::ErrorClass;
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_app_api::{AppError, AppResult, Document};
use spot_events_contract::lifecycle::Operation;
use spot_events_contract::{EventMeta, PayloadDocument, Topic};
use spot_storage_contract::{NodeRef, OutboxEvent};
use time::format_description::well_known::Rfc3339;

use super::IdGenerator;

/// The platform topic of an enqueued job. The job engine (WP-PLT-06) reads these from the
/// outbox; `data` holds `job`, `args`, and, when set, `idempotency_key` and `not_before`
/// (RFC 3339). See [`job_from_event`].
pub const TOPIC_JOB: &str = "sys_job_enqueued";

/// The platform topic of a realtime notice to one user (ADR-25). `data` holds `user` and
/// `payload`. See [`notice_from_event`].
pub const TOPIC_NOTICE: &str = "sys_user_notice";

/// The platform topic of a secret operation's audit event (see [`super::secrets`]). `data` names
/// the operation, DocType, field, caller kind, site, outcome and error code; the actor is in the
/// event's metadata. It never holds a value, a sealed blob or an index.
pub const TOPIC_SECRET_AUDIT: &str = "sys_secret_audit";

/// The audit event of one secret operation, about `subject` (the record, or for an operation
/// that names none, a fresh id).
pub(crate) fn secret_audit(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    subject: NodeRef,
    data: Json,
) -> AppResult<OutboxEvent> {
    build_for(
        ids,
        inv,
        &platform_topic(TOPIC_SECRET_AUDIT)?,
        subject,
        None,
        1,
        data,
    )
}

/// The platform topic of an operation's receipt (see [`super::receipt`]).
pub(crate) fn receipt(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    doc: &Document,
    version: Version,
    data: Json,
) -> AppResult<OutboxEvent> {
    build(
        ids,
        inv,
        &platform_topic(super::receipt::TOPIC_RECEIPT)?,
        doc,
        Some(version),
        1,
        data,
    )
}

/// The platform topic of one external effect announced after the commit.
pub(crate) fn receipt_effect(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    doc: &Document,
    version: Version,
    data: Json,
) -> AppResult<OutboxEvent> {
    build(
        ids,
        inv,
        &platform_topic(super::receipt::TOPIC_RECEIPT_EFFECT)?,
        doc,
        Some(version),
        1,
        data,
    )
}

/// The `doc_state` event (WP-JRN-02.T2) of `doc` at `version`, with `data` already built.
pub(crate) fn doc_state(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    doc: &Document,
    version: Version,
    data: Json,
) -> AppResult<OutboxEvent> {
    let topic = Topic::any(super::capture::TOPIC_DOC_STATE)
        .map_err(|_| capture_error("The state topic is not valid."))?;
    build(ids, inv, &topic, doc, Some(version), 1, data)
        .map_err(|_| capture_error("The state event could not be written."))
}

fn capture_error(what: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "capture.event_failed",
        what.to_owned(),
    )
}

fn event_error(what: impl std::fmt::Display) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.event",
        format!("The event could not be written: {what}"),
    )
}

fn meta(inv: &Invocation, id: RecordId) -> EventMeta {
    EventMeta {
        occurred_at: inv.now,
        actor: inv.actor.clone(),
        command_id: inv.command_id.clone(),
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    }
}

fn subject(doc: &Document) -> NodeRef {
    NodeRef {
        entity: doc.doctype().clone(),
        id: doc.id(),
    }
}

fn build(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    topic: &Topic,
    doc: &Document,
    version: Option<Version>,
    schema_version: u32,
    data: Json,
) -> AppResult<OutboxEvent> {
    build_for(ids, inv, topic, subject(doc), version, schema_version, data)
}

fn build_for(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    topic: &Topic,
    subject: NodeRef,
    version: Option<Version>,
    schema_version: u32,
    mut data: Json,
) -> AppResult<OutboxEvent> {
    if let Json::Object(map) = &mut data {
        map.retain(|_, v| !v.is_null());
    }
    let id = ids.next_id();
    PayloadDocument {
        meta: meta(inv, id),
        data,
    }
    .to_outbox_event(id, topic, subject, version, schema_version)
    .map_err(event_error)
}

/// The lifecycle event of `op` (schema v1, ADR-056), about `doc` at `version`.
pub(crate) fn lifecycle(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    op: Operation,
    doc: &Document,
    version: Option<Version>,
    data: Json,
) -> AppResult<OutboxEvent> {
    build(ids, inv, &op.topic(), doc, version, 1, data)
}

/// An app event published by a step, about the document the step runs for.
pub(crate) fn app_event(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    topic: &Topic,
    doc: &Document,
    schema_version: u32,
    data: Json,
) -> AppResult<OutboxEvent> {
    build(ids, inv, topic, doc, doc.version(), schema_version, data)
}

fn platform_topic(name: &str) -> AppResult<Topic> {
    Topic::any(name).map_err(event_error)
}

/// The event that enqueues `job`, raised while `doc` is being handled.
pub(crate) fn job(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    doc: &Document,
    job: &JobRequest,
) -> AppResult<OutboxEvent> {
    let not_before = job
        .not_before
        .map(|t| t.format(&Rfc3339).map_err(event_error))
        .transpose()?;
    let data = json!({
        "job": job.job,
        "args": job.args,
        "idempotency_key": job.idempotency_key,
        "not_before": not_before,
    });
    build(
        ids,
        inv,
        &platform_topic(TOPIC_JOB)?,
        doc,
        doc.version(),
        1,
        data,
    )
}

/// The event that delivers a realtime notice to `user` after commit.
pub(crate) fn notice(
    ids: &dyn IdGenerator,
    inv: &Invocation,
    doc: &Document,
    user: RecordId,
    payload: Json,
) -> AppResult<OutboxEvent> {
    let data = json!({"user": user.to_string(), "payload": payload});
    build(
        ids,
        inv,
        &platform_topic(TOPIC_NOTICE)?,
        doc,
        doc.version(),
        1,
        data,
    )
}

/// The job an outbox event enqueues; `None` for any other topic.
///
/// # Errors
///
/// `platform.event` if the event is a job event whose payload is malformed.
pub fn job_from_event(event: &OutboxEvent) -> AppResult<Option<JobRequest>> {
    if event.topic.as_str() != TOPIC_JOB {
        return Ok(None);
    }
    let payload = PayloadDocument::from_outbox_event(event).map_err(event_error)?;
    let data = &payload.data;
    let name = data
        .get("job")
        .and_then(Json::as_str)
        .ok_or_else(|| event_error("a job event without a job name"))?;
    let not_before = data
        .get("not_before")
        .and_then(Json::as_str)
        .map(|t| OffsetDateTime::parse(t, &Rfc3339).map_err(event_error))
        .transpose()?;
    Ok(Some(JobRequest {
        job: name.to_owned(),
        args: data.get("args").cloned().unwrap_or(Json::Null),
        idempotency_key: data
            .get("idempotency_key")
            .and_then(Json::as_str)
            .map(str::to_owned),
        not_before,
    }))
}

/// The user and payload of a notice event; `None` for any other topic.
///
/// # Errors
///
/// `platform.event` if the event is a notice whose payload is malformed.
pub fn notice_from_event(event: &OutboxEvent) -> AppResult<Option<(RecordId, Json)>> {
    if event.topic.as_str() != TOPIC_NOTICE {
        return Ok(None);
    }
    let payload = PayloadDocument::from_outbox_event(event).map_err(event_error)?;
    let user = payload
        .data
        .get("user")
        .and_then(Json::as_str)
        .and_then(parse_id)
        .ok_or_else(|| event_error("a notice event without a user"))?;
    let body = payload.data.get("payload").cloned().unwrap_or(Json::Null);
    Ok(Some((user, body)))
}

pub(crate) fn parse_id(text: &str) -> Option<RecordId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}
