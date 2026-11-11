//! Where this crate's audited decisions go.
//!
//! The crate never writes the audit chain itself (that needs storage). It builds an
//! [`AuditRecord`] of `spot-audit-chain` and hands it to an [`AuditSink`]. An action that must
//! be audited (an override, a key release) is **not taken** when the sink fails, so a missing
//! audit can never be the way past a control.

use std::collections::BTreeMap;

use spot_audit_chain::{AuditRecord, PayloadValue};

/// The audit sink failed. No detail: the backend logs its own cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditSinkError;

/// Takes one audit record, durably, or fails.
pub trait AuditSink {
    /// Records `record`.
    ///
    /// # Errors
    /// [`AuditSinkError`] when the record is not durably stored.
    fn record(&mut self, record: AuditRecord) -> Result<(), AuditSinkError>;
}

/// A sink that keeps records in memory (tests, and the reference for others).
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    /// The records in order.
    pub records: Vec<AuditRecord>,
    /// When set, every record fails.
    pub fail: bool,
}

impl AuditSink for MemoryAuditSink {
    fn record(&mut self, record: AuditRecord) -> Result<(), AuditSinkError> {
        if self.fail {
            return Err(AuditSinkError);
        }
        self.records.push(record);
        Ok(())
    }
}

/// Builds a record. The payload holds ids, tiers and counts only; `masked` is true because no
/// protected value is ever put in it.
pub(crate) fn audit_record(
    kind: &str,
    actor: &str,
    at_micros: i64,
    fields: &[(&str, PayloadValue)],
) -> AuditRecord {
    let payload: BTreeMap<String, PayloadValue> = fields
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    AuditRecord {
        kind: kind.to_owned(),
        actor: actor.to_owned(),
        on_behalf_of: None,
        subject: None,
        at: at_micros,
        payload,
        masked: true,
    }
}

/// The namespace of every audit actor this crate writes for its own decisions: an override, a
/// key release, a flagged or a quarantined write.
///
/// It is deliberately not `user:`. The stamp parser ([`crate::OfflineStamp::from_record`])
/// trusts the `user:<id>` prefix for the user who acted, so an audit actor in one namespace
/// must never be readable as a user of the other.
pub const ACTOR_NAMESPACE: &str = "offline-identity";

/// The kind of an audit actor that names a person (a supervisor, a user).
pub const ACTOR_USER: &str = "user";
/// The kind of an audit actor that names a node.
pub const ACTOR_NODE: &str = "node";

/// The prefix the stamp's own actor carries: `user:<id>`. Kept unchanged, because a journal
/// written by an earlier node must stay readable.
pub const STAMP_ACTOR_PREFIX: &str = "user:";

/// An audit actor in this crate's namespace: `offline-identity:<kind>:<id>`.
///
/// The `<id>` part may itself contain a colon; [`actor_parts`] splits at the first one.
#[must_use]
pub fn actor(kind: &str, id: &str) -> String {
    format!("{ACTOR_NAMESPACE}:{kind}:{id}")
}

/// Splits an actor written by [`actor`] into its kind and id.
///
/// `None` for every actor outside the namespace, including a stamp's `user:<id>` actor: a
/// hand-built `user:<id>` string is never accepted as one of this crate's audit actors, and a
/// string in this namespace is never accepted as a stamp user.
#[must_use]
pub fn actor_parts(actor: &str) -> Option<(&str, &str)> {
    let rest = actor.strip_prefix(ACTOR_NAMESPACE)?.strip_prefix(':')?;
    let (kind, id) = rest.split_once(':')?;
    if kind.is_empty() || id.is_empty() {
        return None;
    }
    Some((kind, id))
}

/// `user:<id>` for the actor of a stamp's own audit record.
///
/// Only [`crate::OfflineStamp`] uses this prefix; every other record goes through [`actor`].
pub(crate) fn stamp_actor(user: &str) -> String {
    format!("{STAMP_ACTOR_PREFIX}{user}")
}

/// Text payload value.
pub(crate) fn text(s: &str) -> PayloadValue {
    PayloadValue::Text(s.to_owned())
}

/// Integer payload value, saturating at `i64::MAX`.
pub(crate) fn int(v: u64) -> PayloadValue {
    PayloadValue::Int(i64::try_from(v).unwrap_or(i64::MAX))
}
