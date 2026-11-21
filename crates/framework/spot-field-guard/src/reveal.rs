//! The audited reveal (ADR-070 section 12, rules 2 and 9): the only way a masked or
//! protected value leaves as itself on a channel that cannot carry it.
//!
//! One document and one field per call. Every call, allowed or refused, leaves an audit
//! record; a reveal whose record cannot be queued is not released. A refusal is one error
//! for a missing document, a forbidden one, a field that cannot be revealed and a caller
//! without the right, so the answer says nothing about what exists.

use std::sync::Arc;

use spot_app_api::prelude::Value;
use spot_permissions::{DocFacts, Principal};

use crate::error::GuardError;
use crate::guard::FieldGuard;
use crate::records::{AuditRecord, FieldAccessRecord, Offer, Via};

/// A value loaded for a reveal, with the document facts the permission core needs to decide
/// (owner, state, the link values a scope restricts).
#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    /// The stored, unmasked value.
    pub value: Value,
    /// The facts of the document it belongs to.
    pub doc_facts: DocFacts,
}

/// Loads real values. The engine-backed implementation is wired in the server binary; the
/// guard knows nothing of storage.
pub trait ValueSource: Send + Sync {
    /// The unmasked value of one field of one document, or `None` when there is no such
    /// document (or the source cannot tell).
    fn load(&self, doctype: &str, id: &str, field: &str) -> Option<Loaded>;
}

/// The reveal service. Cheap to clone.
#[derive(Clone)]
pub struct RevealService {
    guard: Arc<FieldGuard>,
    values: Arc<dyn ValueSource>,
}

impl std::fmt::Debug for RevealService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevealService").finish_non_exhaustive()
    }
}

impl RevealService {
    /// A service over a guard and a value source.
    #[must_use]
    pub fn new(guard: Arc<FieldGuard>, values: Arc<dyn ValueSource>) -> Self {
        RevealService { guard, values }
    }

    /// Reveals one field of one document.
    ///
    /// Needs the field's `reveal` right on the document ([`spot_permissions::FieldDecision`]
    /// `can_reveal`) and a non-empty `reason`. Always writes an audit record (who, document,
    /// field, reason, allowed or refused, channel `api`); a field of a `trace` group is also
    /// traced.
    ///
    /// # Errors
    ///
    /// - `fieldguard.reason_required` for an empty reason (nothing is looked up);
    /// - `permission.reveal_denied`, the same error for a missing document, a forbidden
    ///   one, an unknown DocType and a caller without the right;
    /// - `fieldguard.audit_unavailable` or `fieldguard.trace_unavailable` when the record
    ///   cannot be queued: nothing is released.
    pub fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        reason: &str,
    ) -> Result<Value, GuardError> {
        if reason.trim().is_empty() {
            return Err(GuardError::reason_required());
        }
        let g = &self.guard;
        let decided = g.protection(doctype).ok().and_then(|prot| {
            let loaded = self.values.load(doctype, id, field)?;
            let facts = prot.field(field);
            let doc = DocFacts {
                doctype: doctype.to_owned(),
                id: Some(id.to_owned()),
                ..loaded.doc_facts.clone()
            };
            let decision = g.engine.field_access(principal, doctype, &facts, &doc);
            decision.can_reveal.then_some(loaded.value)
        });
        // A missing document and a missing right look the same from here on: `value` is
        // `None` for both and the records say refused.
        let traced = self.is_traced_field(doctype, field);
        let value = decided;
        let allowed = value.is_some();

        if traced {
            let record: FieldAccessRecord =
                g.record(principal, doctype, Some(id), field, Via::Api, allowed);
            if let Err(e) = g.emit_trace(vec![record]) {
                self.audit(principal, doctype, id, field, reason, false);
                return Err(e);
            }
        }
        let queued = self.audit(principal, doctype, id, field, reason, allowed);
        match value {
            Some(value) if queued => Ok(value),
            Some(_) => Err(GuardError::audit_unavailable()),
            None => Err(GuardError::reveal_denied()),
        }
    }

    /// Whether `field` is in a traced group, decided from the facts alone so a missing
    /// document still leaves a refused trace record (probing is itself traced).
    fn is_traced_field(&self, doctype: &str, field: &str) -> bool {
        self.guard
            .protection(doctype)
            .is_ok_and(|prot| self.guard.is_traced(&prot, &prot.field(field)))
    }

    fn audit(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        reason: &str,
        allowed: bool,
    ) -> bool {
        self.guard.audit.offer(AuditRecord::Reveal {
            who: principal.user.clone(),
            at: self.guard.clock.now_utc(),
            doctype: doctype.to_owned(),
            id: id.to_owned(),
            field: field.to_owned(),
            reason: reason.to_owned(),
            allowed,
            via: Via::Api,
        }) == Offer::Accepted
    }

    /// Reveals one field of many documents, for a **system actor** holding the field's
    /// `reveal_bulk` right only: a person is refused whatever their grants. The audit record
    /// carries the count, not the documents. Documents that do not exist are absent from the
    /// result.
    ///
    /// # Errors
    ///
    /// `fieldguard.reason_required`; `fieldguard.reveal_bulk_refused` for anyone but a system
    /// actor with the right; `fieldguard.audit_unavailable` / `fieldguard.trace_unavailable`
    /// when a record cannot be queued (nothing is released).
    pub fn reveal_bulk(
        &self,
        principal: &Principal,
        doctype: &str,
        ids: &[String],
        field: &str,
        reason: &str,
    ) -> Result<Vec<(String, Value)>, GuardError> {
        if reason.trim().is_empty() {
            return Err(GuardError::reason_required());
        }
        let g = &self.guard;
        let allowed = principal.is_system
            && g.protection(doctype).is_ok_and(|prot| {
                let facts = prot.field(field);
                let doc = DocFacts {
                    doctype: doctype.to_owned(),
                    ..DocFacts::default()
                };
                g.engine
                    .field_access(principal, doctype, &facts, &doc)
                    .can_reveal_bulk
            });
        let bulk_record = |allowed: bool| AuditRecord::RevealBulk {
            who: principal.user.clone(),
            at: g.clock.now_utc(),
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            reason: reason.to_owned(),
            count: ids.len(),
            allowed,
        };
        if !allowed {
            let _ = g.audit.offer(bulk_record(false));
            return Err(GuardError::bulk_refused());
        }
        let found: Vec<(String, Value)> = ids
            .iter()
            .filter_map(|id| {
                self.values
                    .load(doctype, id, field)
                    .map(|l| (id.clone(), l.value))
            })
            .collect();
        if self.is_traced_field(doctype, field) {
            let records = found
                .iter()
                .map(|(id, _)| g.record(principal, doctype, Some(id), field, Via::Api, true))
                .collect();
            if let Err(e) = g.emit_trace(records) {
                let _ = g.audit.offer(bulk_record(false));
                return Err(e);
            }
        }
        if g.audit.offer(bulk_record(true)) != Offer::Accepted {
            return Err(GuardError::audit_unavailable());
        }
        Ok(found)
    }
}
