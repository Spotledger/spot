//! The excluded-field view: the sender side and the receiver side.
//!
//! `exclude_fields` names the fields that never leave the origin. The rule is **by name**, at
//! every level: an excluded name is removed whether it names a parent field, a whole child table,
//! or a field inside a child row (`sync_policy.exclude_fields` carries no `table.field` syntax;
//! `SyncPolicy::excludes` answers for a bare name). [`outgoing_view`] builds the copy a sender
//! ships: the excluded names are absent, not blanked. [`accept_incoming`] is the receiver's guard:
//! a document that still carries an excluded name is refused rather than applied, because a field
//! the policy excludes must never appear at a destination.

use std::collections::BTreeMap;

use spot_app_api::{ChildRow, Document};

use crate::error::codes;
use crate::lint::PolicyProblem;
use crate::policy::SyncPolicy;

/// The outgoing view of a document: the same doctype, id, version and state, with every excluded
/// name — and every excluded child table — left out.
///
/// The removal is by name at every level, the same rule the hub's JSON redactor applies:
///
/// * a parent field the policy excludes is absent;
/// * a child table the policy excludes is absent as a whole;
/// * inside every remaining child table, each row keeps its `id` and its row order, and the
///   fields whose name the policy excludes are removed from the row.
///
/// A row whose every field is excluded is **kept**, with an empty field map: dropping it would
/// change what the receiver believes the document is (row counts and ids are part of the
/// document). A field the policy does not exclude is copied untouched.
#[must_use]
pub fn outgoing_view(doc: &Document, policy: &SyncPolicy) -> Document {
    let mut out = match doc.version() {
        Some(version) => Document::loaded(
            doc.doctype().clone(),
            doc.id(),
            version,
            doc.state(),
            BTreeMap::new(),
            BTreeMap::new(),
        ),
        None => Document::new(doc.doctype().clone(), doc.id()),
    };

    for (name, value) in doc.fields() {
        if !policy.excludes(name.as_str()) {
            out.set(name.clone(), value.clone());
        }
    }

    for (table, rows) in doc.tables() {
        if policy.excludes(table.as_str()) {
            continue;
        }
        // Each row is copied with its excluded fields dropped, `id` and order preserved.
        let kept: Vec<ChildRow> = rows
            .iter()
            .map(|row| ChildRow {
                id: row.id,
                fields: row
                    .fields
                    .iter()
                    .filter(|(name, _)| !policy.excludes(name.as_str()))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            })
            .collect();
        out.rows_mut(table.clone()).extend(kept);
    }

    out
}

/// The receiver's guard: refuse a document that still carries a name the policy excludes.
///
/// The rule is by name at every level, the same rule [`outgoing_view`] applies: an excluded
/// parent field, an excluded child table, and an excluded field inside a child row are all
/// refused. A well-behaved sender never sends one; a document that has one is a bug or a hostile
/// peer, so it is refused rather than applied. Every offending name is reported, so one round trip
/// tells the sender what to fix.
///
/// A table the policy excludes is reported once, as the table: the whole table must not arrive, so
/// its row fields are not named separately.
///
/// # Errors
///
/// One [`PolicyProblem`] per excluded parent field and excluded child table the document carries,
/// and one per excluded field found inside a child row, all with code
/// [`codes::EXCLUDED_FIELD_RECEIVED`]. The detail names the table, the row id and the field — never
/// the value that was carried.
pub fn accept_incoming(
    doctype: &str,
    doc: &Document,
    policy: &SyncPolicy,
) -> Result<(), Vec<PolicyProblem>> {
    let mut problems = Vec::new();
    for name in doc.fields().keys() {
        if policy.excludes(name.as_str()) {
            problems.push(PolicyProblem::new(
                codes::EXCLUDED_FIELD_RECEIVED,
                doctype,
                format!("received document carries excluded field `{name}`"),
            ));
        }
    }
    for (table, rows) in doc.tables() {
        if policy.excludes(table.as_str()) {
            problems.push(PolicyProblem::new(
                codes::EXCLUDED_FIELD_RECEIVED,
                doctype,
                format!("received document carries excluded child table `{table}`"),
            ));
            continue;
        }
        for row in rows {
            for field in row.fields.keys() {
                if policy.excludes(field.as_str()) {
                    problems.push(PolicyProblem::new(
                        codes::EXCLUDED_FIELD_RECEIVED,
                        doctype,
                        format!(
                            "received document carries excluded field `{field}` in child table \
                             `{table}` row `{}`",
                            row.id
                        ),
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
