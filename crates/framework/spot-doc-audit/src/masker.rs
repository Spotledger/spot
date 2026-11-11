//! How a diff is masked: the stored (audit) form and the reader-facing history form.
//!
//! ADR-070 section 12 rule 9 says an audit row of a secured field stores the **masked** form; the
//! real value lives only where the field lives. [`FieldMasker::mask_for_audit`] therefore masks
//! every protected field unconditionally, whatever the acting identity's rights, while
//! [`FieldMasker::redact_diff`] is the field guard's own `Channel::Version` redaction for the
//! reader-facing [`history`](crate::history) view.

use std::sync::Arc;

use spot_activity::{FieldChange, REDACTED};
use spot_app_api::prelude::Value;
use spot_field_guard::source::{DocProtection, ProtectionSource};
use spot_field_guard::{FieldGuard, RedactedDiff};
use spot_permissions::{DocFacts, FieldFacts, MaskFormat, Principal, mask};

use crate::error::DocAuditError;

/// A diff masked for storage: protected fields in their masked form, unmapped tables dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditMask {
    /// The changes to store, in the input order.
    pub changes: Vec<FieldChange>,
    /// Some changes were dropped (a table the protection source cannot map).
    pub hidden_changes: bool,
}

/// Masks a diff for the audit row and redacts it for a reader.
pub trait FieldMasker: Send + Sync {
    /// The stored (masked) form of a diff: every protected field masked, whatever the actor.
    ///
    /// # Errors
    /// [`DocAuditError::UnknownDoctype`] when the protection source does not know the DocType.
    fn mask_for_audit(
        &self,
        doctype: &str,
        changes: &[FieldChange],
    ) -> Result<AuditMask, DocAuditError>;

    /// The reader-facing form of a stored diff, through the field guard's `Channel::Version`.
    ///
    /// # Errors
    /// [`DocAuditError::Guard`] when the guard refuses.
    fn redact_diff(
        &self,
        principal: &Principal,
        doctype: &str,
        facts: &DocFacts,
        changes: &[FieldChange],
    ) -> Result<RedactedDiff, DocAuditError>;

    /// Whether `principal` may read the document at all.
    fn can_read(&self, principal: &Principal, doctype: &str, facts: &DocFacts) -> bool;
}

/// The production masker: the field guard plus its protection source.
pub struct GuardMasker {
    guard: Arc<FieldGuard>,
    source: Arc<dyn ProtectionSource>,
}

impl GuardMasker {
    /// A masker over `guard` and the same protection facts the guard reads.
    #[must_use]
    pub fn new(guard: Arc<FieldGuard>, source: Arc<dyn ProtectionSource>) -> Self {
        Self { guard, source }
    }

    /// The field guard.
    #[must_use]
    pub fn guard(&self) -> &Arc<FieldGuard> {
        &self.guard
    }
}

/// Where a diff path points.
enum Target<'a> {
    /// A field of the document: `customer`.
    Field(&'a str),
    /// A column of one child row: `items[id=<uuid>].qty`.
    RowColumn {
        /// The table field name.
        table: &'a str,
        /// The column name.
        column: &'a str,
    },
    /// A whole row added or removed: `items[id=<uuid>]`.
    Row {
        /// The table field name.
        table: &'a str,
    },
}

fn target(path: &str) -> Target<'_> {
    match path.split_once('[') {
        None => Target::Field(path),
        Some((table, rest)) => match rest.split_once(']') {
            Some((_, after)) => match after.strip_prefix('.') {
                Some(column) if !column.is_empty() => Target::RowColumn { table, column },
                _ => Target::Row { table },
            },
            None => Target::Row { table },
        },
    }
}

/// The mask a protected field is stored under: its declared format, or the placeholder.
fn audit_format(facts: &FieldFacts) -> Option<MaskFormat> {
    if facts.groups.is_empty() && facts.mask.is_none() {
        None
    } else {
        Some(facts.mask.unwrap_or(MaskFormat::Placeholder))
    }
}

/// One side masked for storage: text through the core's `mask`; a value without a text form
/// becomes the diff's redaction marker.
fn mask_side(side: &Option<Value>, format: &MaskFormat) -> Option<Value> {
    side.as_ref().map(|v| match v {
        Value::Text(t) => Value::Text(mask(t, format)),
        Value::Null => Value::Null,
        _ => Value::Text(REDACTED.to_owned()),
    })
}

/// A JSON value masked: strings through `mask`, everything else to null.
fn mask_json(v: &serde_json::Value, format: &MaskFormat) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(mask(s, format)),
        _ => serde_json::Value::Null,
    }
}

/// A whole-row snapshot with its protected columns masked. `None` when the snapshot cannot be
/// read, so the caller drops the change (fail closed).
fn mask_row_snapshot(prot: &DocProtection, value: &Option<Value>) -> Option<Option<Value>> {
    let Some(value) = value else {
        return Some(None);
    };
    let Value::Json(text) = value else {
        return None;
    };
    let mut json: serde_json::Value = serde_json::from_str(text).ok()?;
    let fields = json.get_mut("fields")?.as_object_mut()?;
    let names: Vec<String> = fields.keys().cloned().collect();
    for name in names {
        if let Some(format) = audit_format(&prot.field(&name))
            && let Some(v) = fields.get(&name)
        {
            fields.insert(name, mask_json(v, &format));
        }
    }
    Some(Some(Value::Json(json.to_string())))
}

impl FieldMasker for GuardMasker {
    fn mask_for_audit(
        &self,
        doctype: &str,
        changes: &[FieldChange],
    ) -> Result<AuditMask, DocAuditError> {
        let prot = self
            .source
            .protection(doctype)
            .ok_or_else(|| DocAuditError::UnknownDoctype(doctype.to_owned()))?;
        let mut out = AuditMask {
            changes: Vec::new(),
            hidden_changes: false,
        };
        for change in changes {
            let (owner, column) = match target(&change.path) {
                Target::Field(name) => (Arc::clone(&prot), name.to_owned()),
                Target::RowColumn { table, column } => {
                    let Some(child) = prot.child_doctype(table) else {
                        out.hidden_changes = true;
                        continue;
                    };
                    let Some(child_prot) = self.source.protection(child) else {
                        out.hidden_changes = true;
                        continue;
                    };
                    (child_prot, column.to_owned())
                }
                Target::Row { table } => {
                    let Some(child) = prot.child_doctype(table) else {
                        out.hidden_changes = true;
                        continue;
                    };
                    let Some(child_prot) = self.source.protection(child) else {
                        out.hidden_changes = true;
                        continue;
                    };
                    match (
                        mask_row_snapshot(&child_prot, &change.before),
                        mask_row_snapshot(&child_prot, &change.after),
                    ) {
                        (Some(before), Some(after)) => out.changes.push(FieldChange {
                            path: change.path.clone(),
                            before,
                            after,
                        }),
                        _ => out.hidden_changes = true,
                    }
                    continue;
                }
            };
            match audit_format(&owner.field(&column)) {
                None => out.changes.push(change.clone()),
                Some(format) => out.changes.push(FieldChange {
                    path: change.path.clone(),
                    before: mask_side(&change.before, &format),
                    after: mask_side(&change.after, &format),
                }),
            }
        }
        Ok(out)
    }

    fn redact_diff(
        &self,
        principal: &Principal,
        doctype: &str,
        facts: &DocFacts,
        changes: &[FieldChange],
    ) -> Result<RedactedDiff, DocAuditError> {
        self.guard
            .redact_diff(principal, doctype, facts, changes)
            .map_err(|e| DocAuditError::Guard(e.to_string()))
    }

    fn can_read(&self, principal: &Principal, _doctype: &str, facts: &DocFacts) -> bool {
        self.guard
            .engine()
            .can(principal, spot_permissions::Action::Read, facts)
            .allowed
    }
}
