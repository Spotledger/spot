//! The secret audit journal in the audit chain (WP-SEC-03.T7, follow-up F1 of WP-SEC-03.T4).
//!
//! The document engine journals every secret operation as a `sys_secret_audit` outbox event
//! (`spot_document::engine`, `engine/secrets.rs`). Until this module the appender chained only
//! `sys_doc_audit`, so those rows sat in the outbox but not in the tamper-evident chain. Now the
//! appender chains both topics into the same lane chains, in outbox order, once.
//!
//! # What a chain entry holds
//!
//! - `kind` is `secret.<operation>` (`secret.set`, `secret.open`, `secret.find`,
//!   `secret.rewrap`, `secret.is_set`), so a reader tells it from a `doc.*` entry by the kind
//!   alone; the payload also names the topic (`topic = "sys_secret_audit"`).
//! - `actor` and `on_behalf_of` are the event's actor, `at` the invocation's time.
//! - `subject` is the event's subject: the DocType and the record (a fresh id for an operation
//!   that names no record, exactly as the outbox row has).
//! - the payload is the [`SecretAuditEvent`] as JSON text, its digest, and the topic.
//!
//! # No value can get in
//!
//! The decode is an allow-list: [`SECRET_EVENT_KEYS`] is every key the engine writes, each a
//! short text. A secret audit event with any other key, a non-text value, an over-long text, an
//! operation or caller kind the engine does not define, an error that is not a dotted code, an
//! unknown outcome or state is refused (and, being unchainable, stays in the outbox and stops
//! its lane, visibly, until it is dealt with). A DocType, field or site that is not name-shaped
//! is kept as `<invalid>`, so a refused call with odd text cannot stop the lane. The masker has
//! nothing to mask: there is no field of a document in the entry.
//!
//! The limit, stated plainly: this checks the **shape** of every key, so a sealed blob, an
//! index, a structured value or a free sentence cannot pass. It cannot prove that a name-shaped
//! DocType or field text is not secretly a value; those two come from the caller of the secret
//! path (a platform service), are clipped by the engine, and are the same text the outbox row
//! has had since WP-SEC-03.T4.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_audit_chain::{AuditRecord, ChainEntry, PayloadValue, Subject};
use spot_document::secrets::{SecretCaller, SecretOp};
use spot_events_contract::payload::PayloadDocument;
use spot_storage_contract::feed::OutboxRecord;

use crate::error::DocAuditError;
use crate::event::{ActorRef, sha256_hex};
use crate::step::micros;

/// The outbox topic of a secret operation's audit event (the engine's `TOPIC_SECRET_AUDIT`).
pub const SECRET_AUDIT_TOPIC: &str = spot_document::engine::TOPIC_SECRET_AUDIT;

/// The chain kind prefix of a secret audit entry.
pub const SECRET_KIND_PREFIX: &str = "secret.";

/// Every key of a secret audit event's data, and the only keys accepted. None can hold a value:
/// each is the name of an operation, a DocType, a field, a caller kind, a site, a record id, an
/// outcome, an error code or the `<set>` / `<unset>` marker.
pub const SECRET_EVENT_KEYS: &[&str] = &[
    "op", "doctype", "field", "record", "caller", "site", "outcome", "error", "state",
];

/// The longest text any key may hold. The engine clips DocType and field names to 128
/// characters; a longer text is not one of its events.
const MAX_TEXT: usize = 256;

/// The `outcome` of a call that succeeded.
const OUTCOME_OK: &str = "ok";
/// The `outcome` of a call that was refused.
const OUTCOME_REFUSED: &str = "refused";

/// The `state` markers the engine writes.
const STATES: [&str; 3] = ["<set>", "<unset>", "<none>"];

/// A secret audit event as the chain carries it: the fields of the outbox row, nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAuditEvent {
    /// The operation: `set`, `open`, `find`, `rewrap` or `is_set`.
    pub op: String,
    /// The DocType the call named (clipped by the engine).
    pub doctype: String,
    /// The field, or `*` for a whole rewrap batch.
    pub field: String,
    /// The record, when the call named one.
    pub record: Option<String>,
    /// The caller kind of the capability (`login_service`, `seed`, ...).
    pub caller: String,
    /// The site of the invocation.
    pub site: String,
    /// `ok` or `refused`.
    pub outcome: String,
    /// The stable code the call ended with, when it was refused.
    pub error: Option<String>,
    /// `<set>`, `<unset>` or `<none>`; never a value.
    pub state: String,
    /// The acting identity.
    pub actor: ActorRef,
    /// Unix microseconds (the invocation's time).
    pub at: i64,
    /// The DocType of the outbox event's subject (the lane and the chain subject).
    pub subject_doctype: String,
    /// The record id of the outbox event's subject.
    pub subject_id: String,
}

/// Whether `record` is a secret audit event.
#[must_use]
pub fn is_secret_audit(record: &OutboxRecord) -> bool {
    record.event.topic.as_str() == SECRET_AUDIT_TOPIC
}

fn bad(what: &str) -> DocAuditError {
    DocAuditError::Encoding(format!("a secret audit event {what}"))
}

/// A stable error code: dotted lower-case segments (`secretfield.not_lookup`). A bare word is
/// not one, so a token-shaped value does not pass as a code.
fn code_ok(text: &str) -> bool {
    text.len() <= 128
        && text.contains('.')
        && text.split('.').all(|segment| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// The text the chain keeps for a name the caller supplied (a DocType, a field, a site) when it
/// is not name-shaped. A refused call may carry any text; the entry shows that it was refused,
/// and not the text, and the lane is not stopped by it.
const NOT_A_NAME: &str = "<invalid>";

/// A DocType, field or site name as the chain keeps it: itself when it is name-shaped (ASCII
/// letters, digits and `_ - . *`, 128 at most), else [`NOT_A_NAME`].
fn name_text(text: String) -> String {
    let shaped = !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '*'));
    if shaped { text } else { NOT_A_NAME.to_owned() }
}

/// A record id as the engine writes it (hex digits and dashes).
fn id_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Decodes the secret audit event an outbox record carries.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the record is not a secret audit event of the shape the
/// engine writes: another topic, an unknown key, a value that is not a short text, an unknown
/// outcome or state. The message names no payload content.
pub fn decode_secret_outbox(record: &OutboxRecord) -> Result<SecretAuditEvent, DocAuditError> {
    if !is_secret_audit(record) {
        return Err(bad("is expected on its own topic"));
    }
    let doc =
        PayloadDocument::from_outbox_event(&record.event).map_err(|_| bad("could not be read"))?;
    let Json::Object(data) = &doc.data else {
        return Err(bad("has no data object"));
    };
    if data
        .keys()
        .any(|k| !SECRET_EVENT_KEYS.contains(&k.as_str()))
    {
        return Err(bad("carries a field this build does not chain"));
    }
    let text = |key: &str| -> Result<Option<String>, DocAuditError> {
        match data.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) if s.chars().count() <= MAX_TEXT => Ok(Some(s.clone())),
            Some(_) => Err(bad("holds a field that is not a short text")),
        }
    };
    let need = |key: &str| text(key)?.ok_or_else(|| bad("lacks a required field"));
    let op = need("op")?;
    let caller = need("caller")?;
    let outcome = need("outcome")?;
    let state = need("state")?;
    let error = text("error")?;
    let record_id = text("record")?;
    // The engine's own enumerations decide what an operation and a caller kind are.
    if SecretOp::from_name(&op).is_none() || SecretCaller::from_name(&caller).is_none() {
        return Err(bad("names an operation or caller this build does not know"));
    }
    if outcome != OUTCOME_OK && outcome != OUTCOME_REFUSED {
        return Err(bad("has an unknown outcome"));
    }
    if !STATES.contains(&state.as_str()) {
        return Err(bad("has an unknown state"));
    }
    if error.as_deref().is_some_and(|e| !code_ok(e))
        || record_id.as_deref().is_some_and(|r| !id_ok(r))
    {
        return Err(bad("has an error or record in an unknown form"));
    }
    Ok(SecretAuditEvent {
        op,
        doctype: name_text(need("doctype")?),
        field: name_text(need("field")?),
        record: record_id,
        caller,
        site: name_text(need("site")?),
        outcome,
        error,
        state,
        actor: ActorRef::of(&doc.meta.actor),
        at: micros(doc.meta.occurred_at),
        subject_doctype: record.event.subject.entity.as_str().to_owned(),
        subject_id: record.event.subject.id.to_string(),
    })
}

/// The JSON text of an event, as stored in the chain entry.
///
/// # Errors
/// [`DocAuditError::Encoding`] when serialisation fails.
pub fn encode_secret_event(event: &SecretAuditEvent) -> Result<String, DocAuditError> {
    serde_json::to_string(event).map_err(|e| DocAuditError::Encoding(e.to_string()))
}

/// The chain record of a secret audit event: kind `secret.<op>`, the event's subject, the event
/// as JSON text with its digest and the topic as the payload, `masked = true` (there is nothing
/// in it to mask).
///
/// # Errors
/// [`DocAuditError::Chain`] when the record does not validate (for instance a kind that is not
/// a dotted lower-case code); [`DocAuditError::Encoding`] when the event cannot be encoded.
pub fn secret_record_of(event: &SecretAuditEvent) -> Result<AuditRecord, DocAuditError> {
    let json = encode_secret_event(event)?;
    let digest = sha256_hex(json.as_bytes());
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert(
        "topic".to_owned(),
        PayloadValue::Text(SECRET_AUDIT_TOPIC.to_owned()),
    );
    payload.insert("secret_event".to_owned(), PayloadValue::Text(json));
    payload.insert("digest".to_owned(), PayloadValue::Text(digest));
    let record = AuditRecord {
        kind: format!("{SECRET_KIND_PREFIX}{}", event.op),
        actor: event.actor.actor.clone(),
        on_behalf_of: event.actor.on_behalf_of.clone(),
        subject: Some(Subject {
            doctype: event.subject_doctype.clone(),
            id: event.subject_id.clone(),
        }),
        at: event.at,
        payload,
        masked: true,
    };
    record.validate()?;
    Ok(record)
}

/// The secret audit event stored in a chain entry.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the entry carries no secret audit event.
pub fn secret_event_of(entry: &ChainEntry) -> Result<SecretAuditEvent, DocAuditError> {
    match entry.record.payload.get("secret_event") {
        Some(PayloadValue::Text(json)) => {
            serde_json::from_str(json).map_err(|e| DocAuditError::Encoding(e.to_string()))
        }
        _ => Err(DocAuditError::Encoding(
            "the chain entry carries no secret audit event".to_owned(),
        )),
    }
}

/// Whether a chain entry is a secret audit entry (by its kind).
#[must_use]
pub fn is_secret_entry(entry: &ChainEntry) -> bool {
    entry.record.kind.starts_with(SECRET_KIND_PREFIX)
}
