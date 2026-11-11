//! What a consumer may see: the read filter and the field-level filter.
//!
//! A stream consumer with a data subscription carries values, so it is **`Channel::Export`-like**
//! (ADR-070 section 12) and this crate treats it as such: the field guard is asked with
//! [`Channel::Export`], the stricter of the two candidate channels. `Channel::Journal` (names
//! only) would be wrong here because a `fields` consumer receives values; `Channel::Export`
//! refuses what the owner may not report rather than silently degrading, which is what a data
//! subscription needs. The choice is stated here, once, and every projection goes through it.
//!
//! Two questions are answered for every event:
//!
//! - [`Visibility::may_read`]: may the consumer's owner read the subject document at all? An
//!   event about a document the owner may not read is **skipped**, counted in
//!   [`Batch::skipped`](crate::Batch::skipped) and never delivered. The skip count reveals no
//!   subject.
//! - [`Visibility::project`]: for a `fields` consumer, the payload's protected fields are
//!   removed or masked by the one field guard ([`spot_field_guard::FieldGuard`]).
//!
//! The read decision needs the document's facts (creator, state, restricted values). The host
//! injects a [`SubjectFacts`] source; the engine loads nothing itself. The owner's
//! [`Principal`] comes from a [`PrincipalSource`].

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value as Json};
use spot_app_api::Document;
use spot_app_api::prelude::Decimal;
use spot_field_guard::{Channel, FieldGuard};
use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};
use spot_storage_contract::{Name, RecordId, Value};
use time::format_description::well_known::Rfc3339;

use crate::error::StreamError;

/// The subject of an event, as the visibility questions name it.
pub use spot_events_contract::envelope::Subject;

/// The document facts a read decision is asked against, loaded by the host.
pub trait SubjectFacts: Send + Sync {
    /// The facts for one document; `DocFacts::default()` when the host knows none (the
    /// decision then reads as a denial for a non-system principal).
    fn facts(&self, doctype: &str, id: &RecordId) -> DocFacts;
}

/// The principal that registered (and owns) a consumer.
pub trait PrincipalSource: Send + Sync {
    /// The principal for an owner name.
    fn principal(&self, owner: &str) -> Principal;
}

/// The permission questions the engine asks for every event.
pub trait Visibility: Send + Sync {
    /// Whether the owner may read the event's subject document.
    ///
    /// # Errors
    ///
    /// [`StreamError::Permission`] when the decision cannot be made.
    fn may_read(&self, owner: &str, subject: &Subject) -> Result<bool, StreamError>;

    /// Removes or masks the protected fields of `data` in place, for a `fields` consumer.
    ///
    /// # Errors
    ///
    /// [`StreamError::Permission`] when the guard fails; on any error the map is emptied so a
    /// caller that ignores the error still leaks nothing.
    fn project(
        &self,
        owner: &str,
        subject: &Subject,
        data: &mut Map<String, Json>,
    ) -> Result<(), StreamError>;
}

/// A visibility that admits everything and projects nothing: the platform acting as itself,
/// and the tests that do not exercise permissions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllVisible;

impl Visibility for AllVisible {
    fn may_read(&self, _owner: &str, _subject: &Subject) -> Result<bool, StreamError> {
        Ok(true)
    }

    fn project(
        &self,
        _owner: &str,
        _subject: &Subject,
        _data: &mut Map<String, Json>,
    ) -> Result<(), StreamError> {
        Ok(())
    }
}

/// The real visibility: `can(Read)` for the filter, the field guard for the projection.
pub struct PermissionVisibility {
    engine: Arc<PermissionEngine>,
    guard: FieldGuard,
    facts: Arc<dyn SubjectFacts>,
    principals: Arc<dyn PrincipalSource>,
}

impl PermissionVisibility {
    /// A visibility over `engine`, the field `guard`, and the host's fact and principal
    /// sources.
    #[must_use]
    pub fn new(
        engine: Arc<PermissionEngine>,
        guard: FieldGuard,
        facts: Arc<dyn SubjectFacts>,
        principals: Arc<dyn PrincipalSource>,
    ) -> Self {
        Self {
            engine,
            guard,
            facts,
            principals,
        }
    }

    /// The document facts for a subject, with the DocType and id filled in.
    fn doc_facts(&self, subject: &Subject) -> DocFacts {
        let mut facts = self.facts.facts(&subject.entity, &subject.id);
        facts.doctype = subject.entity.clone();
        facts.id = Some(subject.id.to_string());
        facts
    }
}

impl Visibility for PermissionVisibility {
    fn may_read(&self, owner: &str, subject: &Subject) -> Result<bool, StreamError> {
        let principal = self.principals.principal(owner);
        let facts = self.doc_facts(subject);
        Ok(self.engine.can(&principal, Action::Read, &facts).allowed)
    }

    fn project(
        &self,
        owner: &str,
        subject: &Subject,
        data: &mut Map<String, Json>,
    ) -> Result<(), StreamError> {
        let principal = self.principals.principal(owner);
        let facts = self.doc_facts(subject);
        let mut doc = match document_from_json(&subject.entity, &subject.id, data) {
            Ok(doc) => doc,
            Err(e) => {
                data.clear();
                return Err(e);
            }
        };
        match self.guard.redact(
            Channel::Export,
            &principal,
            &subject.entity,
            &facts,
            &mut doc,
        ) {
            Ok(()) => {
                *data = json_from_document(&doc);
                Ok(())
            }
            Err(e) => {
                data.clear();
                Err(StreamError::Permission {
                    code: e.code(),
                    message: e.to_string(),
                })
            }
        }
    }
}

/// Builds a [`Document`] from an event payload's JSON object. A key that is not a storage name,
/// or a value that cannot be represented, drops the field rather than the whole event.
///
/// Nested objects and arrays are kept as a JSON field (`Value::Json`), so a nested protected
/// field is guarded as part of the field it lives in; the event payload's own value encoding
/// keeps decimals as strings, which round-trips unchanged.
fn document_from_json(
    doctype: &str,
    id: &RecordId,
    data: &Map<String, Json>,
) -> Result<Document, StreamError> {
    let name = Name::new(doctype).map_err(|_| StreamError::Permission {
        code: spot_field_guard::error::UNKNOWN_DOCTYPE,
        message: "the subject entity is not a storage name".to_owned(),
    })?;
    let mut doc = Document::new(name, *id);
    for (key, value) in data {
        if let Ok(field) = Name::new(key) {
            doc.set(field, json_to_value(value));
        }
    }
    Ok(doc)
}

/// Serialises a redacted [`Document`] back to the payload's JSON object.
fn json_from_document(doc: &Document) -> Map<String, Json> {
    let mut out = Map::new();
    for (name, value) in doc.fields() {
        out.insert(name.as_str().to_owned(), value_to_json(value));
    }
    out
}

/// A JSON value as a storage value, keeping the native API value encoding.
fn json_to_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                i64::try_from(u).map_or_else(|_| decimal_or_text(&n.to_string()), Value::Int)
            } else {
                decimal_or_text(&n.to_string())
            }
        }
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(_) | Json::Object(_) => Value::Json(value.to_string()),
    }
}

fn decimal_or_text(text: &str) -> Value {
    Decimal::from_str(text).map_or_else(|_| Value::Text(text.to_owned()), Value::Decimal)
}

/// A storage value as a JSON value in the native API value encoding (decimals as strings).
fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::Number((*i).into()),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::DateTime(t) => t.format(&Rfc3339).map_or_else(
            |_| Json::String(t.unix_timestamp().to_string()),
            Json::String,
        ),
        Value::Json(text) => Json::from_str(text).unwrap_or(Json::Null),
        Value::Bytes(b) => Json::String(URL_SAFE_NO_PAD.encode(b)),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form until
        // the native API encoding defines one.
        Value::GeoPoint(_) | Value::Range(_) => Json::String(format!("{value:?}")),
    }
}

/// A [`SubjectFacts`] over a plain map, for tests and simple hosts.
#[derive(Debug, Default)]
pub struct MapSubjectFacts {
    facts: BTreeMap<String, DocFacts>,
}

impl MapSubjectFacts {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the facts for one `(doctype, id)` pair.
    #[must_use]
    pub fn with(mut self, doctype: &str, id: &RecordId, facts: DocFacts) -> Self {
        self.facts.insert(format!("{doctype}:{id}"), facts);
        self
    }
}

impl SubjectFacts for MapSubjectFacts {
    fn facts(&self, doctype: &str, id: &RecordId) -> DocFacts {
        self.facts
            .get(&format!("{doctype}:{id}"))
            .cloned()
            .unwrap_or_default()
    }
}

/// A [`PrincipalSource`] over a plain map, for tests and simple hosts.
#[derive(Debug, Default)]
pub struct MapPrincipals {
    principals: BTreeMap<String, Principal>,
}

impl MapPrincipals {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one owner's principal.
    #[must_use]
    pub fn with(mut self, owner: &str, principal: Principal) -> Self {
        self.principals.insert(owner.to_owned(), principal);
        self
    }
}

impl PrincipalSource for MapPrincipals {
    fn principal(&self, owner: &str) -> Principal {
        self.principals
            .get(owner)
            .cloned()
            .unwrap_or_else(|| Principal::user(owner, Vec::new()))
    }
}
