//! The producer's part of an audit entry: [`AuditRecord`] and its canonical bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::canonical::{put_field, put_opt_field};
use crate::error::AuditError;

/// Maximum encoded size of a record's payload, in bytes.
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
/// Maximum length of the kind, actor, subject and payload-key strings.
const MAX_NAME_BYTES: usize = 256;

/// One payload value. There is deliberately no float variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadValue {
    /// Text.
    Text(String),
    /// A signed integer.
    Int(i64),
    /// A boolean.
    Bool(bool),
}

impl PayloadValue {
    /// The one funnel for adapters converting loosely typed values: floats are refused.
    ///
    /// # Errors
    /// Always [`AuditError::FloatRefused`].
    pub fn from_f64(_value: f64) -> Result<Self, AuditError> {
        Err(AuditError::FloatRefused)
    }
}

/// A payload: keys in ascending byte order, which is the canonical order.
pub type Payload = BTreeMap<String, PayloadValue>;

/// What an audited thing happened to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// The document type.
    pub doctype: String,
    /// The document id.
    pub id: String,
}

/// What a producer asks to have audited.
///
/// Secured values must already be in their masked form (ADR-070 section 12 rule 9); this crate
/// never sees a real value, and `masked` is the producer's assertion that that holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// A dotted lower-case code, such as `agent.decision` or `doc.update`.
    pub kind: String,
    /// `system`, `user:<id>` or `agent:<id>`.
    pub actor: String,
    /// With impersonation, the identity acted on behalf of (same form as `actor`).
    pub on_behalf_of: Option<String>,
    /// The document the record is about, if any.
    pub subject: Option<Subject>,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
    /// Text, integer and boolean values only.
    pub payload: Payload,
    /// The producer asserts every protected payload value is already masked.
    pub masked: bool,
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME_BYTES
}

fn kind_ok(kind: &str) -> bool {
    name_ok(kind)
        && kind.split('.').all(|seg| {
            let mut chars = seg.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn actor_ok(actor: &str) -> bool {
    name_ok(actor)
        && (actor == "system"
            || actor
                .strip_prefix("user:")
                .or_else(|| actor.strip_prefix("agent:"))
                .is_some_and(|id| !id.is_empty()))
}

/// The exact number of bytes `payload` occupies in the canonical encoding.
#[must_use]
pub fn payload_encoded_len(payload: &Payload) -> usize {
    let mut out = Vec::new();
    encode_payload(&mut out, payload);
    out.len()
}

fn encode_payload(out: &mut Vec<u8>, payload: &Payload) {
    let count = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for (key, value) in payload {
        put_field(out, key.as_bytes());
        match value {
            PayloadValue::Text(t) => {
                out.push(1);
                put_field(out, t.as_bytes());
            }
            PayloadValue::Int(i) => {
                out.push(2);
                out.extend_from_slice(&i.to_be_bytes());
            }
            PayloadValue::Bool(b) => {
                out.push(3);
                out.push(u8::from(*b));
            }
        }
    }
}

impl AuditRecord {
    /// Checks the record is in its documented form and within the payload limit.
    ///
    /// # Errors
    /// [`AuditError::RecordInvalid`] or [`AuditError::PayloadTooLarge`].
    pub fn validate(&self) -> Result<(), AuditError> {
        if !kind_ok(&self.kind) {
            return Err(AuditError::RecordInvalid(
                "kind is not a dotted lower-case code",
            ));
        }
        if !actor_ok(&self.actor) {
            return Err(AuditError::RecordInvalid(
                "actor is not system, user:<id> or agent:<id>",
            ));
        }
        if self.on_behalf_of.as_deref().is_some_and(|a| !actor_ok(a)) {
            return Err(AuditError::RecordInvalid(
                "on_behalf_of is not system, user:<id> or agent:<id>",
            ));
        }
        if let Some(s) = &self.subject
            && !(name_ok(&s.doctype) && name_ok(&s.id))
        {
            return Err(AuditError::RecordInvalid(
                "subject has an empty or long part",
            ));
        }
        if self.payload.keys().any(|k| !name_ok(k)) {
            return Err(AuditError::RecordInvalid(
                "a payload key is empty or too long",
            ));
        }
        let size = payload_encoded_len(&self.payload);
        if size > MAX_PAYLOAD_BYTES {
            return Err(AuditError::PayloadTooLarge {
                size,
                limit: MAX_PAYLOAD_BYTES,
            });
        }
        Ok(())
    }

    /// The canonical bytes (format version 1) the entry hash covers.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        put_field(&mut out, self.kind.as_bytes());
        put_field(&mut out, self.actor.as_bytes());
        put_opt_field(&mut out, self.on_behalf_of.as_deref().map(str::as_bytes));
        match &self.subject {
            None => out.push(0),
            Some(s) => {
                out.push(1);
                put_field(&mut out, s.doctype.as_bytes());
                put_field(&mut out, s.id.as_bytes());
            }
        }
        out.extend_from_slice(&self.at.to_be_bytes());
        out.push(u8::from(self.masked));
        encode_payload(&mut out, &self.payload);
        out
    }
}
