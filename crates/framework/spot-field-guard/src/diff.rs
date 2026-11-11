//! The outbound redactor for `spot-activity` diffs (version history and audit diffs).
//!
//! A diff is built from a before and an after image, so a protected field's old **and** new
//! value are both in it. [`FieldGuard::redact_diff`] masks both sides of a masked field,
//! drops the change of a hidden field and keeps a marker that something changed which the
//! reader cannot see, so the history does not lie by omission.

use std::sync::Arc;

use spot_activity::{FieldChange, REDACTED};
use spot_app_api::prelude::Value;
use spot_permissions::{DocFacts, MaskFormat, Principal, mask};

use crate::channel::Channel;
use crate::error::GuardError;
use crate::guard::{FieldGuard, Shown};
use crate::records::{FieldAccessRecord, Via};

/// A diff as one reader may see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactedDiff {
    /// The changes the reader may see, in the input order, masked where the field is masked.
    pub changes: Vec<FieldChange>,
    /// Some fields changed that the reader cannot see. A flag, not a count or a name: the
    /// marker tells the history is incomplete and nothing else.
    pub hidden_changes: bool,
}

/// Where a change path points.
enum Target<'a> {
    /// A field of the document: `customer`.
    Field(&'a str),
    /// A column of one child row: `items[id=<uuid>].qty`.
    RowColumn { table: &'a str, column: &'a str },
    /// A whole row added or removed: `items[id=<uuid>]`.
    Row { table: &'a str },
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

/// A masked side of a change: text through the core's `mask`; a value without a text form
/// becomes the diff's own redaction marker (it says only that a value exists).
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

impl FieldGuard {
    /// Redacts a diff for [`Channel::Version`].
    ///
    /// - a **masked** field: both the old and the new value are masked;
    /// - a **hidden** field: the change is dropped and
    ///   [`RedactedDiff::hidden_changes`] is set;
    /// - a whole **row** added or removed: the row snapshot's columns are redacted one by one
    ///   (a hidden column is removed from the snapshot and sets the marker);
    /// - a table the source cannot map to a child DocType, or a row snapshot that does not
    ///   parse, is dropped whole (fail closed).
    ///
    /// A traced field whose real value stays in the output is traced as authorised, one that
    /// is dropped as refused.
    ///
    /// # Errors
    ///
    /// An unknown DocType, or a full trace queue under `fail_closed`.
    pub fn redact_diff(
        &self,
        principal: &Principal,
        doctype: &str,
        doc_facts: &DocFacts,
        changes: &[FieldChange],
    ) -> Result<RedactedDiff, GuardError> {
        let channel = Channel::Version;
        let via = Via::Channel(channel);
        let prot = self.protection(doctype)?;
        let id = doc_facts.id.clone();
        let mut out = RedactedDiff {
            changes: Vec::new(),
            hidden_changes: false,
        };
        let mut records: Vec<FieldAccessRecord> = Vec::new();
        for change in changes {
            let (table, column) = match target(&change.path) {
                Target::Field(name) => (None, name),
                Target::RowColumn { table, column } => (Some(table), column),
                Target::Row { table } => (Some(table), ""),
            };
            let owner = match table {
                None => doctype.to_owned(),
                Some(t) => {
                    let checked = self.check(channel, principal, doctype, &prot, t, doc_facts);
                    match (checked.shown, prot.child_doctype(t)) {
                        (Shown::Value, Some(child)) => child.to_owned(),
                        _ => {
                            out.hidden_changes = true;
                            continue;
                        }
                    }
                }
            };
            let owner_prot = if owner == doctype {
                Arc::clone(&prot)
            } else {
                self.protection(&owner)?
            };
            if column.is_empty() {
                // A whole row: redact its snapshot column by column.
                let row_doc = DocFacts {
                    doctype: owner.clone(),
                    ..DocFacts::default()
                };
                let mut hidden = false;
                let before = self.redact_row_side(
                    &change.before,
                    channel,
                    principal,
                    &owner,
                    &owner_prot,
                    &row_doc,
                    &mut hidden,
                );
                let after = self.redact_row_side(
                    &change.after,
                    channel,
                    principal,
                    &owner,
                    &owner_prot,
                    &row_doc,
                    &mut hidden,
                );
                match (before, after) {
                    (Some(before), Some(after)) => {
                        if hidden {
                            out.hidden_changes = true;
                        }
                        out.changes.push(FieldChange {
                            path: change.path.clone(),
                            before,
                            after,
                        });
                    }
                    _ => out.hidden_changes = true,
                }
                continue;
            }
            let facts_doc = if owner == doctype {
                doc_facts.clone()
            } else {
                DocFacts {
                    doctype: owner.clone(),
                    ..DocFacts::default()
                }
            };
            let c = self.check(channel, principal, &owner, &owner_prot, column, &facts_doc);
            let trace_path = match table {
                Some(t) => format!("{t}.{column}"),
                None => column.to_owned(),
            };
            match c.shown {
                Shown::Value => {
                    if c.traced {
                        records.push(self.record(
                            principal,
                            doctype,
                            id.as_deref(),
                            &trace_path,
                            via,
                            true,
                        ));
                    }
                    out.changes.push(change.clone());
                }
                Shown::Masked(format) => out.changes.push(FieldChange {
                    path: change.path.clone(),
                    before: mask_side(&change.before, &format),
                    after: mask_side(&change.after, &format),
                }),
                Shown::Hidden => {
                    if c.traced {
                        records.push(self.record(
                            principal,
                            doctype,
                            id.as_deref(),
                            &trace_path,
                            via,
                            false,
                        ));
                    }
                    out.hidden_changes = true;
                }
            }
        }
        self.emit_trace(records)?;
        Ok(out)
    }

    /// One side of a row change (a JSON row snapshot) with its columns redacted. `None`
    /// means the side could not be redacted and the change must be dropped; the outer
    /// `Option` of the result keeps an absent side absent.
    #[allow(clippy::too_many_arguments)]
    fn redact_row_side(
        &self,
        side: &Option<Value>,
        channel: Channel,
        principal: &Principal,
        child: &str,
        child_prot: &crate::source::DocProtection,
        row_doc: &DocFacts,
        hidden: &mut bool,
    ) -> Option<Option<Value>> {
        let Some(value) = side else {
            return Some(None);
        };
        let Value::Json(text) = value else {
            return None;
        };
        let mut json: serde_json::Value = serde_json::from_str(text).ok()?;
        let fields = json.get_mut("fields")?.as_object_mut()?;
        let names: Vec<String> = fields.keys().cloned().collect();
        for name in names {
            let c = self.check(channel, principal, child, child_prot, &name, row_doc);
            match c.shown {
                Shown::Value => {}
                Shown::Masked(format) => {
                    if let Some(v) = fields.get(&name) {
                        let masked = mask_json(v, &format);
                        fields.insert(name, masked);
                    }
                }
                Shown::Hidden => {
                    fields.remove(&name);
                    *hidden = true;
                }
            }
        }
        Some(Some(Value::Json(json.to_string())))
    }
}
