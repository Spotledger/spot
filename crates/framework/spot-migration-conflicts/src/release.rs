//! The release items a detection run reasons about, read from the plan.
//!
//! The planner (WP-MIG-01) already knows what the release changes. This module reads the parts
//! the conflict checks need out of [`ReleasePlan::changes`] — the DocTypes and fields it removes,
//! the fields it adds and the fields it renames — and turns each into a [`Cause`] and a suggested
//! resolution. Nothing here re-diffs anything: the plan is the source of truth.

use std::collections::BTreeMap;

use spot_migration_plan::{ChangeKind, ReleasePlan};

use crate::model::Cause;

/// How a release breaks one field reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FieldBreak {
    /// The field is removed; `change` is the plan change's id.
    Removed {
        /// The plan change id.
        change: String,
    },
    /// The field is renamed; the reference should point at `to`.
    Renamed {
        /// The new field name.
        to: String,
        /// The plan change id.
        change: String,
    },
}

/// The release items the conflict checks read from the plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReleaseDelta {
    removed_doctypes: BTreeMap<String, String>,
    removed_fields: BTreeMap<(String, String), String>,
    renamed_fields: BTreeMap<(String, String), (String, String)>,
    added_fields: BTreeMap<(String, String), String>,
    tightened_fields: BTreeMap<(String, String), (String, String)>,
}

impl ReleaseDelta {
    /// Reads the delta out of a plan's changes.
    pub(crate) fn from_plan(plan: &ReleasePlan) -> Self {
        let mut delta = Self::default();
        for change in &plan.changes {
            match change.kind {
                ChangeKind::DocTypeRemoved => {
                    delta
                        .removed_doctypes
                        .insert(change.target.clone(), change.id.clone());
                }
                ChangeKind::FieldRemoved => {
                    if let Some(key) = field_key(&change.target) {
                        delta.removed_fields.insert(key, change.id.clone());
                    }
                }
                ChangeKind::FieldAdded => {
                    if let Some(key) = field_key(&change.target) {
                        delta.added_fields.insert(key, change.id.clone());
                    }
                }
                ChangeKind::FieldRenamed => {
                    let Some((doctype, to)) = field_key(&change.target) else {
                        continue;
                    };
                    let from = change
                        .before
                        .clone()
                        .unwrap_or_else(|| change.target.clone());
                    delta
                        .renamed_fields
                        .insert((doctype, from), (to, change.id.clone()));
                }
                ChangeKind::FieldRequiredChanged if change.after.as_deref() == Some("true") => {
                    if let Some(key) = field_key(&change.target) {
                        delta.tightened_fields.insert(
                            key,
                            (
                                change.id.clone(),
                                format!("the field `{}` becomes required", change.target),
                            ),
                        );
                    }
                }
                ChangeKind::FieldDefaultChanged
                    if change.before.as_deref().is_some_and(|d| !d.is_empty())
                        && change.after.as_deref().unwrap_or("").is_empty() =>
                {
                    if let Some(key) = field_key(&change.target) {
                        delta.tightened_fields.insert(
                            key,
                            (
                                change.id.clone(),
                                format!("the default of `{}` is removed", change.target),
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
        delta
    }

    /// How the release breaks a reference to `doctype.field`, if it does.
    pub(crate) fn field_break(&self, doctype: &str, field: &str) -> Option<FieldBreak> {
        let key = (doctype.to_owned(), field.to_owned());
        if let Some(change) = self.removed_fields.get(&key) {
            return Some(FieldBreak::Removed {
                change: change.clone(),
            });
        }
        self.renamed_fields
            .get(&key)
            .map(|(to, change)| FieldBreak::Renamed {
                to: to.clone(),
                change: change.clone(),
            })
    }

    /// The plan change id that added `doctype.field`, when the release adds it.
    pub(crate) fn field_added(&self, doctype: &str, field: &str) -> Option<&str> {
        self.added_fields
            .get(&(doctype.to_owned(), field.to_owned()))
            .map(String::as_str)
    }

    /// The cause of a broken reference to a field named `field` on **some** DocType, for a rule
    /// that names no DocType. The first DocType in name order that removed the name wins, so the
    /// answer is deterministic.
    pub(crate) fn field_cause_by_name(&self, field: &str) -> Option<(Cause, String)> {
        let doctype = self
            .removed_fields
            .keys()
            .find(|(_, name)| name == field)
            .map(|(doctype, _)| doctype.clone())?;
        self.field_cause(&doctype, field)
    }

    /// The cause of a removed DocType, and the resolution a site owner has.
    pub(crate) fn doctype_cause(&self, doctype: &str) -> Option<(Cause, String)> {
        let change = self.removed_doctypes.get(doctype)?;
        Some((
            Cause::new(change, format!("the DocType `{doctype}` is removed")),
            format!("Remove this customisation, or keep the DocType `{doctype}` in the release."),
        ))
    }

    /// The cause of a broken field reference, and the resolution a site owner has.
    pub(crate) fn field_cause(&self, doctype: &str, field: &str) -> Option<(Cause, String)> {
        match self.field_break(doctype, field)? {
            FieldBreak::Removed { change } => Some((
                Cause::new(change, format!("the field `{doctype}.{field}` is removed")),
                format!(
                    "Point this customisation at another field, or keep `{doctype}.{field}` in the release."
                ),
            )),
            FieldBreak::Renamed { to, change } => Some((
                Cause::new(
                    change,
                    format!("the field `{doctype}.{field}` is renamed to `{to}`"),
                ),
                format!("Rename the reference from `{field}` to `{to}`."),
            )),
        }
    }

    /// The cause when the release tightens `doctype.field` — it becomes required or loses its
    /// default — which can strand a save when a view hides it.
    pub(crate) fn tightened_cause(&self, doctype: &str, field: &str) -> Option<(Cause, String)> {
        let (change, effect) = self
            .tightened_fields
            .get(&(doctype.to_owned(), field.to_owned()))?;
        Some((
            Cause::new(change, effect),
            "Give the hidden field a default, or stop hiding it in the view.".to_owned(),
        ))
    }
}

/// Splits a plan change target `doctype.field` into its parts. A target with no `.` is not a
/// field target.
fn field_key(target: &str) -> Option<(String, String)> {
    let (doctype, field) = target.split_once('.')?;
    if doctype.is_empty() || field.is_empty() {
        return None;
    }
    Some((doctype.to_owned(), field.to_owned()))
}
