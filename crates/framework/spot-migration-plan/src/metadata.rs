//! The metadata diff: DocTypes, fields, naming, protection, permissions and view layers.
//!
//! Every comparison is between the **effective** definitions of the two systems (app metadata
//! merged with site customisations). A rename is only ever inferred from an explicit
//! [`crate::model::RenameHint`]; without one a rename is reported as a removal plus an
//! addition and flagged, because the plan cannot tell the two apart. Weakening protection is
//! always at least [`crate::risk::Risk::NeedsReview`].

use std::collections::{BTreeMap, BTreeSet};

use spot_meta::{FieldSpec, FieldType};

use crate::change::{BlockingClass, Change, ChangeKind};
use crate::model::{DocTypeDescription, SystemDescription};
use crate::risk::{RiskTable, rules};

/// Diffs the effective metadata of `from` against `to`.
pub fn diff(from: &SystemDescription, to: &SystemDescription, table: &RiskTable) -> Vec<Change> {
    let mut out = Vec::new();
    let from_map: BTreeMap<&str, &DocTypeDescription> = from
        .doctypes
        .iter()
        .map(|d| (d.definition.name.as_str(), d))
        .collect();
    let to_map: BTreeMap<&str, &DocTypeDescription> = to
        .doctypes
        .iter()
        .map(|d| (d.definition.name.as_str(), d))
        .collect();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(from_map.keys().copied());
    names.extend(to_map.keys().copied());

    for name in names {
        match (from_map.get(name), to_map.get(name)) {
            (None, Some(dt)) => out.push(doctype_added(dt, table)),
            (Some(dt), None) => out.push(doctype_removed(dt, table)),
            (Some(a), Some(b)) => doctype_changed(a, b, to, table, &mut out),
            (None, None) => {}
        }
    }
    out
}

/// A DocType that appears.
fn doctype_added(dt: &DocTypeDescription, table: &RiskTable) -> Change {
    let rule = rules::METADATA_DOCTYPE_ADDED;
    Change::new(
        format!("metadata/{}/doctype_added", dt.definition.name),
        ChangeKind::DocTypeAdded,
        rule,
        dt.definition.name.clone(),
        format!("DocType `{}` is added.", dt.definition.name),
        table.risk_of(rule),
        BlockingClass::BriefLock,
    )
}

/// A DocType that disappears.
fn doctype_removed(dt: &DocTypeDescription, table: &RiskTable) -> Change {
    let rule = rules::METADATA_DOCTYPE_REMOVED;
    Change::new(
        format!("metadata/{}/doctype_removed", dt.definition.name),
        ChangeKind::DocTypeRemoved,
        rule,
        dt.definition.name.clone(),
        format!("DocType `{}` is removed.", dt.definition.name),
        table.risk_of(rule),
        BlockingClass::MaintenanceWindow,
    )
    .why("removing a DocType discards its records; the backend refuses it online")
    .alternative("keep the DocType and stop using it, then remove it in a later release")
}

/// A DocType present in both systems.
fn doctype_changed(
    from: &DocTypeDescription,
    to: &DocTypeDescription,
    to_system: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let name = &to.definition.name;
    let hints: Vec<&crate::model::RenameHint> = to_system
        .rename_hints
        .iter()
        .filter(|h| &h.doctype == name)
        .collect();

    fields(from, to, name, &hints, table, out);
    if from.definition.naming != to.definition.naming {
        let rule = rules::METADATA_NAMING_CHANGED;
        let mut c = Change::new(
            format!("metadata/{name}/naming"),
            ChangeKind::NamingChanged,
            rule,
            name.clone(),
            format!("The naming of `{name}` changes."),
            table.risk_of(rule),
            BlockingClass::BriefLock,
        );
        if let (Some(a), Some(b)) = (&from.definition.naming, &to.definition.naming) {
            c = c.evidence(a.clone(), b.clone());
        }
        c = c.why("renaming a series after documents exist is a data migration");
        out.push(c);
    }
    permissions(from, to, name, table, out);
    views(from, to, name, table, out);
}

/// The field-level changes of one DocType.
fn fields(
    from: &DocTypeDescription,
    to: &DocTypeDescription,
    doctype: &str,
    hints: &[&crate::model::RenameHint],
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_fields: BTreeMap<&str, &FieldSpec> = from
        .definition
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    let to_fields: BTreeMap<&str, &FieldSpec> = to
        .definition
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();

    let mut renamed_from: BTreeSet<&str> = BTreeSet::new();
    let mut renamed_to: BTreeSet<&str> = BTreeSet::new();
    for hint in hints {
        let (Some(old), Some(new)) = (
            from_fields.get(hint.from.as_str()),
            to_fields.get(hint.to.as_str()),
        ) else {
            continue;
        };
        renamed_from.insert(hint.from.as_str());
        renamed_to.insert(hint.to.as_str());
        let rule = rules::METADATA_FIELD_RENAMED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/field_renamed/{}", hint.to),
                ChangeKind::FieldRenamed,
                rule,
                format!("{doctype}.{}", hint.to),
                format!(
                    "Field `{doctype}.{}` is renamed to `{}`.",
                    hint.from, hint.to
                ),
                table.risk_of(rule),
                BlockingClass::MaintenanceWindow,
            )
            .evidence(hint.from.clone(), hint.to.clone())
            .why("a rename moves stored values; the backend cannot do it online")
            .alternative(
                "add the new field, backfill it, keep the old one, then drop it in a later release",
            ),
        );
        let _ = (old, new);
    }

    for (fname, spec) in &to_fields {
        if renamed_to.contains(fname) {
            continue;
        }
        if !from_fields.contains_key(fname) {
            out.push(field_added(doctype, spec, table));
        }
    }
    for (fname, spec) in &from_fields {
        if renamed_from.contains(fname) {
            continue;
        }
        if !to_fields.contains_key(fname) {
            out.push(field_removed(doctype, spec, table));
        }
    }
    for (fname, old) in &from_fields {
        let Some(new) = to_fields.get(fname) else {
            continue;
        };
        field_changed(doctype, old, new, table, out);
    }
}

/// A field that appears.
fn field_added(doctype: &str, spec: &FieldSpec, table: &RiskTable) -> Change {
    let rule = rules::METADATA_FIELD_ADDED;
    Change::new(
        format!("metadata/{doctype}/field_added/{}", spec.name),
        ChangeKind::FieldAdded,
        rule,
        format!("{doctype}.{}", spec.name),
        format!("Field `{doctype}.{}` is added.", spec.name),
        table.risk_of(rule),
        BlockingClass::BriefLock,
    )
    .evidence("absent", format!("{:?}", spec.ty))
}

/// A field that disappears.
fn field_removed(doctype: &str, spec: &FieldSpec, table: &RiskTable) -> Change {
    let rule = rules::METADATA_FIELD_REMOVED;
    Change::new(
        format!("metadata/{doctype}/field_removed/{}", spec.name),
        ChangeKind::FieldRemoved,
        rule,
        format!("{doctype}.{}", spec.name),
        format!("Field `{doctype}.{}` is removed.", spec.name),
        table.risk_of(rule),
        BlockingClass::MaintenanceWindow,
    )
    .evidence(format!("{:?}", spec.ty), "absent")
    .why("removing a field discards its values and may be a rename without a hint")
    .alternative(
        "if this is a rename, add a rename hint; otherwise stop using the field first and drop it later",
    )
}

/// A field present in both systems.
fn field_changed(
    doctype: &str,
    old: &FieldSpec,
    new: &FieldSpec,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let target = format!("{doctype}.{}", new.name);
    if old.ty != new.ty {
        let rule = rules::METADATA_FIELD_RETYPED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/field_retyped/{}", new.name),
                ChangeKind::FieldRetyped,
                rule,
                target.clone(),
                format!("Field `{target}` changes type."),
                table.risk_of(rule),
                BlockingClass::MaintenanceWindow,
            )
            .evidence(format!("{:?}", old.ty), format!("{:?}", new.ty))
            .why("changing a field's type can lose values")
            .alternative(
                "add a new field of the new type, backfill it, then drop the old one later",
            ),
        );
    }
    if matches!(new.ty, FieldType::Link | FieldType::DynamicLink)
        && old.options != new.options
        && old.ty == new.ty
    {
        let rule = rules::METADATA_LINK_TARGET_CHANGED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/link_target/{}", new.name),
                ChangeKind::LinkTargetChanged,
                rule,
                target.clone(),
                format!("The link target of `{target}` changes."),
                table.risk_of(rule),
                BlockingClass::MaintenanceWindow,
            )
            .evidence(
                old.options.clone().unwrap_or_default(),
                new.options.clone().unwrap_or_default(),
            )
            .why("existing values may point at records that do not exist in the new target"),
        );
    }
    if old.required != new.required {
        let rule = if new.required {
            rules::METADATA_FIELD_REQUIRED_SET
        } else {
            rules::METADATA_FIELD_REQUIRED_CLEARED
        };
        let mut c = Change::new(
            format!("metadata/{doctype}/field_required/{}", new.name),
            ChangeKind::FieldRequiredChanged,
            rule,
            target.clone(),
            format!(
                "Field `{target}` becomes {}.",
                if new.required { "required" } else { "optional" }
            ),
            table.risk_of(rule),
            if new.required {
                BlockingClass::BriefLock
            } else {
                BlockingClass::Online
            },
        )
        .evidence(old.required.to_string(), new.required.to_string());
        if new.required {
            c = c
                .why("existing records may have no value for it")
                .alternative(
                    "add it optional, backfill it, then make it required in a later release",
                );
        }
        out.push(c);
    }
    if old.default != new.default {
        let rule = rules::METADATA_FIELD_DEFAULT_CHANGED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/field_default/{}", new.name),
                ChangeKind::FieldDefaultChanged,
                rule,
                target.clone(),
                format!("The default of `{target}` changes."),
                table.risk_of(rule),
                BlockingClass::Online,
            )
            .evidence(
                old.default.clone().unwrap_or_default(),
                new.default.clone().unwrap_or_default(),
            ),
        );
    }
    if protection_differs(old, new) {
        let rule = rules::METADATA_PROTECTION_CHANGED;
        let mut c = Change::new(
            format!("metadata/{doctype}/protection/{}", new.name),
            ChangeKind::ProtectionChanged,
            rule,
            target.clone(),
            format!("The protection of `{target}` changes."),
            table.risk_of(rule),
            BlockingClass::Online,
        )
        .evidence(protection_text(old), protection_text(new));
        if protection_weakens(old, new) {
            c = c
                .why("protection is weakened: a field becomes less protected or its mask reveals more")
                .alternative("keep the protection or mask as it was until a review approves the change");
        } else {
            c = c.why("a protection or mask change needs review");
        }
        out.push(c);
    }
}

/// The permission-rule changes of one DocType.
fn permissions(
    from: &DocTypeDescription,
    to: &DocTypeDescription,
    doctype: &str,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_set: BTreeSet<&str> = from.permissions.iter().map(String::as_str).collect();
    let to_set: BTreeSet<&str> = to.permissions.iter().map(String::as_str).collect();
    for rule_text in to_set.difference(&from_set) {
        let rule = rules::METADATA_PERMISSION_ADDED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/permission_added/{}", slug(rule_text)),
                ChangeKind::PermissionRuleAdded,
                rule,
                format!("{doctype} permission"),
                format!("A permission rule is added to `{doctype}`."),
                table.risk_of(rule),
                BlockingClass::Online,
            )
            .evidence("absent", (*rule_text).to_owned())
            .why("a new permission rule changes who may act on the DocType"),
        );
    }
    for rule_text in from_set.difference(&to_set) {
        let rule = rules::METADATA_PERMISSION_REMOVED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/permission_removed/{}", slug(rule_text)),
                ChangeKind::PermissionRuleRemoved,
                rule,
                format!("{doctype} permission"),
                format!("A permission rule is removed from `{doctype}`."),
                table.risk_of(rule),
                BlockingClass::Online,
            )
            .evidence((*rule_text).to_owned(), "absent")
            .why("removing a permission rule may open access it used to restrict"),
        );
    }
}

/// The view-layer changes of one DocType.
fn views(
    from: &DocTypeDescription,
    to: &DocTypeDescription,
    doctype: &str,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(from.views.keys().map(String::as_str));
    keys.extend(to.views.keys().map(String::as_str));
    for key in keys {
        if from.views.get(key) == to.views.get(key) {
            continue;
        }
        let rule = rules::METADATA_VIEW_CHANGED;
        out.push(
            Change::new(
                format!("metadata/{doctype}/view/{key}"),
                ChangeKind::ViewLayerChanged,
                rule,
                format!("{doctype} view {key}"),
                format!("The view layer `{key}` of `{doctype}` changes."),
                table.risk_of(rule),
                BlockingClass::Online,
            )
            .evidence(
                from.views.get(key).cloned().unwrap_or_default(),
                to.views.get(key).cloned().unwrap_or_default(),
            )
            .why("a view layer change needs review"),
        );
    }
}

/// Whether the protection of a field changes at all.
fn protection_differs(old: &FieldSpec, new: &FieldSpec) -> bool {
    old.protection_group != new.protection_group
        || old.masking != new.masking
        || old.classification != new.classification
        || old.declassified != new.declassified
}

/// Whether the change weakens protection.
fn protection_weakens(old: &FieldSpec, new: &FieldSpec) -> bool {
    if old.protection_group.is_some() && new.protection_group.is_none() {
        return true;
    }
    if old.declassified.is_none() && new.declassified.is_some() {
        return true;
    }
    if !old
        .classification
        .iter()
        .all(|t| new.classification.contains(t))
    {
        return true;
    }
    match (&old.masking, &new.masking) {
        (Some(_), None) => true,
        (
            Some(spot_meta::MaskSpec {
                last_chars: None, ..
            }),
            Some(spot_meta::MaskSpec {
                last_chars: Some(_),
                ..
            }),
        ) => true,
        (
            Some(spot_meta::MaskSpec {
                last_chars: Some(a),
                ..
            }),
            Some(spot_meta::MaskSpec {
                last_chars: Some(b),
                ..
            }),
        ) => b > a,
        _ => false,
    }
}

/// A short, stable text of a field's protection, for the evidence.
fn protection_text(spec: &FieldSpec) -> String {
    format!(
        "group={} mask={} tags=[{}] declassified={}",
        spec.protection_group.as_deref().unwrap_or("none"),
        mask_text(spec.masking.as_ref()),
        spec.classification.join(","),
        spec.declassified.as_deref().unwrap_or("none"),
    )
}

/// A mask as readable text.
fn mask_text(mask: Option<&spot_meta::MaskSpec>) -> String {
    match mask {
        None => "none".to_owned(),
        Some(spot_meta::MaskSpec {
            last_chars: Some(n),
            ..
        }) => format!("last_chars({n})"),
        Some(_) => "placeholder".to_owned(),
    }
}

/// A slug for a permission rule's id: lowercase letters, digits and dashes.
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "rule".to_owned()
    } else {
        out
    }
}
