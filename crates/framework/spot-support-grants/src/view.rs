//! The customer's own view of support sessions: who, what scope, what was done, what
//! changed, and whether the tenant's chain still verifies.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use spot_audit_chain::{AuditChain, AuditStore, ChainEntry, KeyRing, PayloadValue, Verifier};

use crate::error::GrantError;
use crate::record::{AuditExecutor, EventKind};

/// One action in a session, as the customer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    /// When the action was attempted (unix seconds).
    pub at: i64,
    /// The action's name.
    pub action: String,
    /// The DocType it reached, if any.
    pub doctype: Option<String>,
    /// The tier it needed.
    pub tier: String,
    /// `allowed` or `denied`.
    pub outcome: String,
    /// The stable decision code.
    pub code: String,
    /// What it changed, in plain words; empty for reads.
    pub change: String,
}

/// One approved extension in a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRecord {
    /// When it was approved (unix seconds).
    pub at: i64,
    /// Who approved it (a customer administrator).
    pub by: String,
    /// The new end instant it approved (unix seconds).
    pub to: i64,
}

/// Everything the customer sees about one support session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    /// The grant/session id.
    pub session: String,
    /// The tenant.
    pub tenant: String,
    /// The subject who acted.
    pub who: String,
    /// `agent` or `person`.
    pub who_kind: String,
    /// The support organisation.
    pub support_org: String,
    /// The tier ceiling of the grant.
    pub tier: String,
    /// The scope in plain words.
    pub scope: String,
    /// When the request was recorded.
    pub requested_at: Option<i64>,
    /// When the customer approved it.
    pub approved_at: Option<i64>,
    /// When the session started.
    pub started_at: Option<i64>,
    /// When the session stopped.
    pub stopped_at: Option<i64>,
    /// When the grant was revoked, if it was.
    pub revoked_at: Option<i64>,
    /// Why it was revoked.
    pub revocation_reason: String,
    /// Every action attempted, in order, with its tier and outcome.
    pub actions: Vec<ActionRecord>,
    /// The plain-language changes made, in order.
    pub changes: Vec<String>,
    /// The approved extensions, in order.
    pub extensions: Vec<ExtensionRecord>,
    /// Whether the tenant's chain verified over the whole range read.
    pub chain_verified: bool,
    /// The chain failures found, as stable `audit.*` codes; empty when it verified.
    pub failures: Vec<String>,
}

/// The customer-side view over the tenant's own audit chain.
///
/// Reading verifies the chain with the customer's trusted checkpoint keys and returns the
/// sessions whose entries fall inside the requested range. A support vendor has no
/// operation that edits the chain (the store is append-only), so a suppressed or altered
/// entry shows up here as `chain_verified: false` with the failing `audit.*` codes.
pub struct CustomerView<'a, S: AuditStore, E: AuditExecutor> {
    chain: &'a AuditChain<S>,
    keys: &'a KeyRing,
    executor: &'a E,
}

impl<'a, S: AuditStore, E: AuditExecutor> CustomerView<'a, S, E> {
    /// A view over the tenant's chain, trusting the keys in `keys`.
    pub fn new(
        chain: &'a AuditChain<S>,
        keys: &'a KeyRing,
        executor: &'a E,
    ) -> CustomerView<'a, S, E> {
        CustomerView {
            chain,
            keys,
            executor,
        }
    }

    /// The sessions with an entry inside `range` (unix seconds, inclusive), each with the
    /// chain verification result.
    ///
    /// # Errors
    ///
    /// [`GrantError::WrongTenant`] when the chain belongs to another tenant, or
    /// [`GrantError::AuditUnavailable`] when the chain cannot be read.
    pub fn sessions(
        &self,
        tenant: &str,
        range: RangeInclusive<i64>,
    ) -> Result<Vec<SessionRecord>, GrantError> {
        if self.chain.site() != tenant {
            return Err(GrantError::WrongTenant {
                asked: tenant.to_owned(),
                grant: self.chain.site().to_owned(),
            });
        }
        let report = self
            .executor
            .run(Verifier::new(self.keys).verify_range(self.chain.store(), tenant, 1, u64::MAX))?
            .map_err(|_| GrantError::AuditUnavailable)?;
        let entries = self
            .executor
            .run(self.chain.read_range(1, u64::MAX))?
            .map_err(|_| GrantError::AuditUnavailable)?;
        let verified = report.is_ok();
        let failures: Vec<String> = report
            .failures
            .iter()
            .map(|failure| failure.kind.code().to_owned())
            .collect();
        let mut records: BTreeMap<String, SessionRecord> = BTreeMap::new();
        for entry in &entries {
            let Some(event) = parse(entry) else {
                continue;
            };
            if !range.contains(&event.at) {
                continue;
            }
            let record = records
                .entry(event.session.clone())
                .or_insert_with(|| SessionRecord {
                    session: event.session.clone(),
                    tenant: tenant.to_owned(),
                    who: String::new(),
                    who_kind: event.who_kind.clone(),
                    support_org: event.support_org.clone(),
                    tier: event.tier.clone(),
                    scope: event.scope.clone(),
                    requested_at: None,
                    approved_at: None,
                    started_at: None,
                    stopped_at: None,
                    revoked_at: None,
                    revocation_reason: String::new(),
                    actions: Vec::new(),
                    changes: Vec::new(),
                    extensions: Vec::new(),
                    chain_verified: verified,
                    failures: failures.clone(),
                });
            if record.who.is_empty() {
                record.who = event.subject.clone();
                record.who_kind = event.subject_kind.clone();
            }
            if !event.scope.is_empty() {
                record.scope = event.scope.clone();
            }
            if !event.support_org.is_empty() {
                record.support_org = event.support_org.clone();
            }
            if !event.tier.is_empty() {
                record.tier = event.tier.clone();
            }
            match event.kind {
                EventKind::Requested => record.requested_at = Some(event.at),
                EventKind::Approved => record.approved_at = Some(event.at),
                EventKind::Started => record.started_at = Some(event.at),
                EventKind::Stopped => record.stopped_at = Some(event.at),
                EventKind::Revoked => {
                    record.revoked_at = Some(event.at);
                    record.revocation_reason = event.reason.clone();
                }
                EventKind::Extended => {
                    if let Some(to) = event.extended_to {
                        record.extensions.push(ExtensionRecord {
                            at: event.at,
                            by: event.who.clone(),
                            to,
                        });
                    }
                }
                EventKind::Action => {
                    record.actions.push(ActionRecord {
                        at: event.at,
                        action: event.action.clone(),
                        doctype: event.doctype.clone(),
                        tier: event.tier.clone(),
                        outcome: event.outcome.clone(),
                        code: event.code.clone(),
                        change: event.change.clone(),
                    });
                    if !event.change.is_empty() {
                        record.changes.push(event.change.clone());
                    }
                }
            }
        }
        let mut sessions: Vec<SessionRecord> = records.into_values().collect();
        sessions.sort_by_key(|record| {
            (
                record
                    .started_at
                    .or(record.requested_at)
                    .unwrap_or(i64::MAX),
                record.session.clone(),
            )
        });
        Ok(sessions)
    }
}

/// One parsed entry, before it is folded into a [`SessionRecord`].
struct Parsed {
    kind: EventKind,
    session: String,
    who: String,
    who_kind: String,
    subject: String,
    subject_kind: String,
    support_org: String,
    tier: String,
    scope: String,
    action: String,
    doctype: Option<String>,
    outcome: String,
    code: String,
    change: String,
    reason: String,
    extended_to: Option<i64>,
    at: i64,
}

fn parse(entry: &ChainEntry) -> Option<Parsed> {
    let subject = entry.record.subject.as_ref()?;
    if subject.doctype != "SupportSession" {
        return None;
    }
    let kind = EventKind::parse(&text(&entry.record.payload, "event")?)?;
    Some(Parsed {
        kind,
        session: subject.id.clone(),
        who: text(&entry.record.payload, "who").unwrap_or_default(),
        who_kind: text(&entry.record.payload, "who_kind").unwrap_or_default(),
        subject: text(&entry.record.payload, "subject")
            .unwrap_or_else(|| text(&entry.record.payload, "who").unwrap_or_default()),
        subject_kind: text(&entry.record.payload, "subject_kind")
            .unwrap_or_else(|| text(&entry.record.payload, "who_kind").unwrap_or_default()),
        support_org: text(&entry.record.payload, "org").unwrap_or_default(),
        tier: text(&entry.record.payload, "tier").unwrap_or_default(),
        scope: text(&entry.record.payload, "scope").unwrap_or_default(),
        action: text(&entry.record.payload, "action").unwrap_or_default(),
        doctype: text(&entry.record.payload, "doctype"),
        outcome: text(&entry.record.payload, "outcome").unwrap_or_default(),
        code: text(&entry.record.payload, "code").unwrap_or_default(),
        change: text(&entry.record.payload, "change").unwrap_or_default(),
        reason: text(&entry.record.payload, "reason").unwrap_or_default(),
        extended_to: integer(&entry.record.payload, "extended_to"),
        at: entry.record.at.div_euclid(1_000_000),
    })
}

fn text(payload: &spot_audit_chain::Payload, key: &str) -> Option<String> {
    match payload.get(key) {
        Some(PayloadValue::Text(value)) => Some(value.clone()),
        _ => None,
    }
}

fn integer(payload: &spot_audit_chain::Payload, key: &str) -> Option<i64> {
    match payload.get(key) {
        Some(PayloadValue::Int(value)) => Some(*value),
        _ => None,
    }
}
