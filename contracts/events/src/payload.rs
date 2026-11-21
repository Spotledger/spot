//! The payload document stored in [`OutboxEvent::payload`]: `{"meta": ..., "data": ...}`.
//!
//! The storage contract stores the payload as JSON text and knows nothing about its structure.
//! This module fixes that structure, so every producer writes the same metadata and every
//! consumer can read it without knowing the topic.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use spot_storage_contract::{Name, NodeRef, OutboxEvent, RecordId, Version};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::names::{CommandId, Topic};

/// Who caused an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Actor {
    /// A signed-in user or API key owner.
    User {
        /// The user's record id: the identity whose rights applied.
        #[serde(with = "record_id")]
        id: RecordId,
        /// The real person, when they acted as `id` (impersonation by the support tier,
        /// ADR-070 section 9). Absent for an ordinary action. Events and audit rows keep both.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_record_id"
        )]
        via: Option<RecordId>,
    },
    /// A consumer principal (ADR-083 section 2.2): a person outside the `User` DocType, acting
    /// on the consumer plane. Never a user, never impersonated (no `via`), never given a role;
    /// what it may do is the surface grants and the owner scope, not this actor.
    Consumer {
        /// The consumer's opaque id. A different principal from a `User` with the same id.
        #[serde(with = "record_id")]
        id: RecordId,
    },
    /// The platform itself: a job, the scheduler, a migration, sync apply.
    System {
        /// Which component, for example `scheduler` or `sync_apply`.
        #[serde(with = "name")]
        component: Name,
    },
}

impl Actor {
    /// The typed principal reference of this actor, or `None` for the platform itself, which is
    /// a component and not a principal.
    #[must_use]
    pub fn principal_ref(&self) -> Option<PrincipalRef> {
        match self {
            Self::User { id, .. } => Some(PrincipalRef::user(*id)),
            Self::Consumer { id } => Some(PrincipalRef::consumer(*id)),
            Self::System { .. } => None,
        }
    }

    /// The text an audit record and a request grant carry for this actor: `user:<id>`,
    /// `consumer:<id>` or `system`. The same id under two kinds is two different texts, so a
    /// grant issued to one never verifies for the other. The real person of an impersonation
    /// (`via`) and the platform component are separate fields, not part of this text.
    #[must_use]
    pub fn principal_text(&self) -> String {
        self.principal_ref()
            .map_or_else(|| SYSTEM_PRINCIPAL.to_owned(), |p| p.to_string())
    }
}

/// The text of the platform itself as an actor (see [`Actor::principal_text`]).
pub const SYSTEM_PRINCIPAL: &str = "system";

/// The kind of a principal reference: which store the id lives in (ADR-083 section 2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum PrincipalKind {
    /// A `User` row of the ERP plane (any `user_type`).
    User,
    /// A consumer principal of the consumer plane.
    Consumer,
}

impl PrincipalKind {
    /// The prefix of the kind's reference text, without the colon.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Consumer => "consumer",
        }
    }

    /// The kind a reference text names, by its prefix and a non-empty id with no white space.
    /// It does **not** parse the id (a request authority only needs the shape; use
    /// [`PrincipalRef`]'s `FromStr` for the strict form).
    #[must_use]
    pub fn of_text(text: &str) -> Option<Self> {
        let (prefix, id) = text.split_once(':')?;
        if id.is_empty() || id.chars().any(char::is_whitespace) {
            return None;
        }
        [Self::User, Self::Consumer]
            .into_iter()
            .find(|k| k.prefix() == prefix)
    }
}

/// A typed principal reference: `user:<id>` or `consumer:<id>` (ADR-083 section 2.2 item 4).
///
/// The text form is canonical (lower-case kind, canonical hyphenated lower-case UUID) and parsing
/// is strict: a bare id, another kind or another spelling is an error, so one principal has
/// exactly one text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrincipalRef {
    kind: PrincipalKind,
    id: RecordId,
}

impl PrincipalRef {
    /// A `user:<id>` reference.
    #[must_use]
    pub fn user(id: RecordId) -> Self {
        Self {
            kind: PrincipalKind::User,
            id,
        }
    }

    /// A `consumer:<id>` reference.
    #[must_use]
    pub fn consumer(id: RecordId) -> Self {
        Self {
            kind: PrincipalKind::Consumer,
            id,
        }
    }

    /// Which store the id lives in.
    #[must_use]
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// The id within that store.
    #[must_use]
    pub fn id(&self) -> RecordId {
        self.id
    }
}

impl std::fmt::Display for PrincipalRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.prefix(), self.id)
    }
}

/// Why a text is not a principal reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalRefError(String);

impl std::fmt::Display for PrincipalRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PrincipalRefError {}

impl std::str::FromStr for PrincipalRef {
    type Err = PrincipalRefError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let kind = PrincipalKind::of_text(text)
            .ok_or_else(|| PrincipalRefError("expected `user:<id>` or `consumer:<id>`".into()))?;
        let id = text
            .split_once(':')
            .map(|(_, id)| id)
            .ok_or_else(|| PrincipalRefError("missing the id".into()))?;
        let id = record_id::parse(id).map_err(PrincipalRefError)?;
        Ok(Self { kind, id })
    }
}

/// Metadata every event carries, whatever its topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventMeta {
    /// When the fact happened: the business transaction's time, in UTC.
    #[serde(with = "utc")]
    pub occurred_at: OffsetDateTime,
    /// Who caused it.
    pub actor: Actor,
    /// The command that caused it, when there was one (the API's `Idempotency-Key`). Lets a
    /// client retrying an uncertain command, and consumers, recognise the retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    /// The event that caused this one, when an event handler produced it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_record_id"
    )]
    pub causation_id: Option<RecordId>,
    /// The root of the chain: the first event of a chain of causes. A root event's correlation
    /// id is its own id.
    #[serde(with = "record_id")]
    pub correlation_id: RecordId,
    /// The W3C trace id (32 lower-case hex digits) of the request or job, for tracing across
    /// consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Why a payload document could not be written or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// The payload is not JSON, or does not have the `{meta, data}` shape.
    Malformed(String),
    /// `data` is not a JSON object.
    DataNotObject,
    /// `trace_id` is not 32 lower-case hex digits.
    InvalidTraceId,
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadError::Malformed(m) => write!(f, "malformed event payload: {m}"),
            PayloadError::DataNotObject => f.write_str("event data must be a JSON object"),
            PayloadError::InvalidTraceId => {
                f.write_str("trace_id must be 32 lower-case hex digits")
            }
        }
    }
}

impl std::error::Error for PayloadError {}

/// The payload document: metadata plus the topic-specific data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadDocument {
    /// Metadata.
    pub meta: EventMeta,
    /// Topic data, a JSON object in the native API value encoding (decimals as strings,
    /// ADR-055), valid against the topic's [`EventSchema`](crate::EventSchema).
    pub data: serde_json::Value,
}

impl PayloadDocument {
    /// Checks the document's own rules: `data` is an object and `trace_id` is well formed.
    pub fn check(&self) -> Result<(), PayloadError> {
        if !self.data.is_object() {
            return Err(PayloadError::DataNotObject);
        }
        if let Some(t) = &self.meta.trace_id {
            let ok = t.len() == 32
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !ok {
                return Err(PayloadError::InvalidTraceId);
            }
        }
        Ok(())
    }

    /// Builds the storage outbox event carrying this document.
    pub fn to_outbox_event(
        &self,
        id: RecordId,
        topic: &Topic,
        subject: NodeRef,
        subject_version: Option<Version>,
        schema_version: u32,
    ) -> Result<OutboxEvent, PayloadError> {
        self.check()?;
        let payload =
            serde_json::to_string(self).map_err(|e| PayloadError::Malformed(e.to_string()))?;
        Ok(OutboxEvent {
            id,
            topic: topic.name().clone(),
            subject,
            subject_version,
            schema_version,
            payload,
        })
    }

    /// Reads the document from a storage outbox event.
    pub fn from_outbox_event(event: &OutboxEvent) -> Result<Self, PayloadError> {
        let doc: PayloadDocument = serde_json::from_str(&event.payload)
            .map_err(|e| PayloadError::Malformed(e.to_string()))?;
        doc.check()?;
        Ok(doc)
    }
}

/// RFC 3339 in UTC, always ending in `Z`; any offset is accepted on input and converted.
pub(crate) mod utc {
    use super::*;

    pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        let text = t
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        s.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let text = String::deserialize(d)?;
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(|t| t.to_offset(time::UtcOffset::UTC))
            .map_err(serde::de::Error::custom)
    }
}

/// A [`RecordId`] as its canonical hyphenated UUIDv7 string.
pub(crate) mod record_id {
    use super::*;

    pub fn serialize<S: Serializer>(id: &RecordId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RecordId, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(serde::de::Error::custom)
    }

    pub fn parse(text: &str) -> Result<RecordId, String> {
        let uuid = uuid::Uuid::try_parse(text).map_err(|e| e.to_string())?;
        if uuid.hyphenated().to_string() != text {
            return Err("a record id must be in canonical lower-case hyphenated form".into());
        }
        RecordId::from_bytes(*uuid.as_bytes()).map_err(|e| e.to_string())
    }
}

/// A storage [`Name`] as its string.
pub(crate) mod name {
    use super::*;

    pub fn serialize<S: Serializer>(n: &Name, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(n.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Name, D::Error> {
        let text = String::deserialize(d)?;
        Name::new(&text).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// An optional [`RecordId`], as a string or absent.
pub(crate) mod opt_record_id {
    use super::*;

    pub fn serialize<S: Serializer>(id: &Option<RecordId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.serialize_str(&id.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RecordId>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| record_id::parse(&t).map_err(serde::de::Error::custom))
            .transpose()
    }
}
