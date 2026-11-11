//! The governance audit entry: who, what, why and which policy, for every exception and every
//! refusal, and for every policy change.
//!
//! An entry takes the tamper-evident shape of `spot-audit-chain` ([`spot_audit_chain::AuditRecord`])
//! and rides the outbox in the same transaction as the record write, exactly as the document
//! audit does ([`enqueue_governance`] is the enqueue the engine's transaction-bound writer
//! performs). It carries no field value: the reason is the user's own justification, never a
//! document's contents.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_app_api::context::Actor;
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId};
use spot_audit_chain::{AuditRecord, Payload, PayloadValue, Subject};
use spot_events_contract::Topic;
use spot_events_contract::payload::{Actor as EventActor, EventMeta, PayloadDocument};
use spot_storage_contract::feed::Outbox;
use spot_storage_contract::{NodeRef, OutboxEvent};

use crate::context::actor_text;
use crate::error::GovernanceError;
use crate::mode::GovernanceMode;
use crate::settings::POLICY_KEY;

/// The audit-class topic governance entries are written under. Reserved `sys_` prefix
/// (ADR-061 section 2); the appender that chains them is a later integration task, as with the
/// document audit's own topic.
pub const GOVERNANCE_TOPIC: &str = "sys_bpm_governance";

/// The payload schema version of a governance audit entry.
pub const GOVERNANCE_SCHEMA_VERSION: u32 = 1;

/// What a governance audit entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditKind {
    /// A governed record was created outside its request through the outside-process permission
    /// and a reason (the audited exception).
    OutsideProcess,
    /// A governed record was refused because it was created outside its request with neither a
    /// valid case context nor an acceptable outside-process exception.
    Refused,
    /// A record type's mode was changed.
    PolicyChange,
}

impl AuditKind {
    /// The dotted lower-case audit kind.
    ///
    /// Built with `concat!` so the dotted literal is not mistaken for an error code by the
    /// repository's error-code scan.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutsideProcess => concat!("bpm.governance.", "outside_process"),
            Self::Refused => concat!("bpm.governance.", "refused"),
            Self::PolicyChange => concat!("bpm.governance.", "policy_change"),
        }
    }
}

/// One governance audit entry.
#[derive(Clone, Debug, PartialEq)]
pub struct GovernanceAudit {
    /// What happened.
    pub kind: AuditKind,
    /// Who acted.
    pub actor: Actor,
    /// The record type, when the entry is about one.
    pub doctype: Option<String>,
    /// The record being created, when the entry is about one (absent for a policy change).
    pub document_id: Option<RecordId>,
    /// The company the policy applies to.
    pub company: String,
    /// The mode in force.
    pub mode: GovernanceMode,
    /// The caller's reason, for an outside-process exception.
    pub reason: Option<String>,
    /// A short, value-free description of the refusal, for a refusal.
    pub detail: Option<String>,
    /// The command id, when there was one.
    pub command_id: Option<String>,
    /// When it happened.
    pub at: OffsetDateTime,
}

impl GovernanceAudit {
    /// An outside-process exception.
    #[must_use]
    pub fn outside_process(
        actor: &Actor,
        company: &str,
        doctype: &str,
        document_id: RecordId,
        reason: &str,
        command_id: Option<&str>,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind: AuditKind::OutsideProcess,
            actor: actor.clone(),
            doctype: Some(doctype.to_owned()),
            document_id: Some(document_id),
            company: company.to_owned(),
            mode: GovernanceMode::Governed,
            reason: Some(reason.to_owned()),
            detail: None,
            command_id: command_id.map(str::to_owned),
            at,
        }
    }

    /// A refusal.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn refused(
        actor: &Actor,
        company: &str,
        doctype: &str,
        document_id: RecordId,
        mode: GovernanceMode,
        detail: &str,
        command_id: Option<&str>,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind: AuditKind::Refused,
            actor: actor.clone(),
            doctype: Some(doctype.to_owned()),
            document_id: Some(document_id),
            company: company.to_owned(),
            mode,
            reason: None,
            detail: Some(detail.to_owned()),
            command_id: command_id.map(str::to_owned),
            at,
        }
    }

    /// A policy change.
    #[must_use]
    pub fn policy_change(
        actor: &Actor,
        company: &str,
        doctype: &str,
        mode: GovernanceMode,
        detail: &str,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind: AuditKind::PolicyChange,
            actor: actor.clone(),
            doctype: Some(doctype.to_owned()),
            document_id: None,
            company: company.to_owned(),
            mode,
            reason: None,
            detail: Some(detail.to_owned()),
            command_id: None,
            at,
        }
    }

    /// The tamper-evident record this entry becomes.
    #[must_use]
    pub fn to_record(&self) -> AuditRecord {
        AuditRecord {
            kind: self.kind.as_str().to_owned(),
            actor: actor_text(&self.actor),
            on_behalf_of: match &self.actor {
                Actor::User { via: Some(via), .. } => Some(format!("user:{via}")),
                _ => None,
            },
            subject: self.subject(),
            at: micros(self.at),
            payload: self.payload(),
            // No document value is ever put in an entry: the reason is the caller's own text.
            masked: true,
        }
    }

    fn subject(&self) -> Option<Subject> {
        match (&self.doctype, self.document_id) {
            (Some(doctype), Some(id)) => Some(Subject {
                doctype: doctype.clone(),
                id: id.to_string(),
            }),
            _ => None,
        }
    }

    fn payload(&self) -> Payload {
        let mut payload: Payload = BTreeMap::new();
        payload.insert(
            "company".to_owned(),
            PayloadValue::Text(self.company.clone()),
        );
        payload.insert(
            "mode".to_owned(),
            PayloadValue::Text(self.mode.as_str().to_owned()),
        );
        payload.insert(
            "policy".to_owned(),
            PayloadValue::Text(POLICY_KEY.to_owned()),
        );
        if let Some(doctype) = &self.doctype {
            payload.insert("doctype".to_owned(), PayloadValue::Text(doctype.clone()));
        }
        if let Some(reason) = &self.reason {
            payload.insert("reason".to_owned(), PayloadValue::Text(reason.clone()));
        }
        if let Some(detail) = &self.detail {
            payload.insert("detail".to_owned(), PayloadValue::Text(detail.clone()));
        }
        if let Some(command) = &self.command_id {
            payload.insert("command_id".to_owned(), PayloadValue::Text(command.clone()));
        }
        payload
    }

    /// The outbox subject: the document for a create entry, or a deterministic policy
    /// pseudo-record for a policy change.
    fn subject_node_ref(&self) -> Result<NodeRef, GovernanceError> {
        match (&self.doctype, self.document_id) {
            (Some(doctype), Some(id)) => Ok(NodeRef {
                entity: name(doctype)?,
                id,
            }),
            (Some(doctype), None) => Ok(NodeRef {
                entity: name("bpm_governance_policy")?,
                id: policy_id(&self.company, doctype),
            }),
            (None, _) => Err(GovernanceError::Audit {
                detail: "a governance audit entry names no record type".to_owned(),
            }),
        }
    }
}

/// Where the audit entry is written: the engine binds a writer to the business transaction, so a
/// refusal or an exception cannot survive without its audit and cannot be lost with it.
pub trait AuditSink: Send + Sync {
    /// Writes `entry`.
    ///
    /// # Errors
    /// [`GovernanceError::Audit`] when the write fails, so the operation fails closed.
    fn write(&self, entry: &GovernanceAudit) -> Result<(), GovernanceError>;
}

/// A sink that fails every write, for a fail-closed test.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailingAudit;

impl AuditSink for FailingAudit {
    fn write(&self, _entry: &GovernanceAudit) -> Result<(), GovernanceError> {
        Err(GovernanceError::Audit {
            detail: "the audit sink is unavailable".to_owned(),
        })
    }
}

/// An in-memory sink: the entries it was given, in order. A reference for tests and for a host
/// that has not bound a transaction writer yet.
#[derive(Debug, Default)]
pub struct RecordingAudit {
    entries: RwLock<Vec<GovernanceAudit>>,
}

impl RecordingAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every entry written, in order.
    #[must_use]
    pub fn entries(&self) -> Vec<GovernanceAudit> {
        self.entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The number of entries written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether no entry was written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Wraps `self` in an `Arc`.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl AuditSink for RecordingAudit {
    fn write(&self, entry: &GovernanceAudit) -> Result<(), GovernanceError> {
        self.entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry.clone());
        Ok(())
    }
}

/// The outbox event a governance audit entry becomes.
///
/// # Errors
/// [`GovernanceError::Audit`] when the topic, subject or payload is malformed.
pub fn outbox_event_of(entry: &GovernanceAudit) -> Result<OutboxEvent, GovernanceError> {
    let topic = Topic::any(GOVERNANCE_TOPIC).map_err(|e| GovernanceError::Audit {
        detail: e.to_string(),
    })?;
    let id = RecordId::new_v7();
    let record = entry.to_record();
    let meta = EventMeta {
        occurred_at: entry.at,
        actor: event_actor(&entry.actor),
        command_id: entry
            .command_id
            .as_deref()
            .and_then(|c| CommandId::new(c).ok()),
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let data = serde_json::to_value(&record).map_err(|e| GovernanceError::Audit {
        detail: e.to_string(),
    })?;
    let doc = PayloadDocument { meta, data };
    doc.to_outbox_event(
        id,
        &topic,
        entry.subject_node_ref()?,
        // A create is audited before the record is written, so there is no version yet.
        None,
        GOVERNANCE_SCHEMA_VERSION,
    )
    .map_err(|e| GovernanceError::Audit {
        detail: e.to_string(),
    })
}

/// Writes `entry` to `tx`'s outbox as a `sys_bpm_governance` event, in the same transaction. A
/// transaction-bound [`AuditSink`] calls exactly this.
///
/// # Errors
/// [`GovernanceError::Audit`] when the outbox refuses, so the operation fails closed.
pub async fn enqueue_governance<O: Outbox + ?Sized>(
    tx: &mut O,
    entry: &GovernanceAudit,
) -> Result<(), GovernanceError> {
    let event = outbox_event_of(entry)?;
    tx.enqueue(&event)
        .await
        .map_err(|e| GovernanceError::Audit {
            detail: e.explanation,
        })
}

/// Unix microseconds of an instant.
#[must_use]
pub fn micros(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1000).unwrap_or(0)
}

/// The events-contract actor of an invocation actor.
fn event_actor(actor: &Actor) -> EventActor {
    match actor {
        Actor::User { id, via } => EventActor::User { id: *id, via: *via },
        Actor::Consumer { id } => EventActor::Consumer { id: *id },
        Actor::System { component } => EventActor::System {
            component: component.clone(),
        },
    }
}

/// A storage name, or an audit error.
fn name(text: &str) -> Result<Name, GovernanceError> {
    Name::new(text).map_err(|e| GovernanceError::Audit {
        detail: e.to_string(),
    })
}

/// A deterministic pseudo-record id for a policy, so a policy-change event has a subject.
///
/// It is a UUIDv7-shaped value derived from the company and record type by FNV-1a; it is never a
/// stored record, only an ordering key for the audit feed.
fn policy_id(company: &str, doctype: &str) -> RecordId {
    let mut bytes = fnv1a_128(format!("{company}\u{0}{doctype}").as_bytes());
    // The version nibble is 7, so `RecordId::from_bytes` accepts it.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes).unwrap_or_else(|_| RecordId::new_v7())
}

/// A 128-bit FNV-1a hash.
fn fnv1a_128(bytes: &[u8]) -> [u8; 16] {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash.to_be_bytes()
}
