//! The private helpers of [`apply_op`](crate::draft::apply_op): the field, child-table and
//! layout edits, the naming and value guard rails, and the type-change application.
//!
//! They live here rather than in `draft.rs` so each file stays a cohesive, readable unit; they
//! are crate-internal and never part of the public API.

use spot_meta::{Definition, FieldSpec, FieldType};

use crate::condition::Condition;
use crate::draft::{
    ChildDraft, Draft, MAX_FIELDS, OpError, data_loss_token, is_protected_doctype,
    is_reserved_field, looks_like_script, looks_like_sql,
};
use crate::layout::FormLayout;
use crate::naming::is_storage_name;
use crate::typechange::{Classification, SampleStats};

/// The `<doctype>.<field>` (or `<child>.<field>`) path of a field.
pub(crate) fn field_path(draft: &Draft, child: Option<&str>, name: &str) -> String {
    match child {
        Some(child) => format!("{child}.{name}"),
        None => format!("{}.{name}", draft.doctype()),
    }
}

/// A mutable field on the root or a child.
pub(crate) fn field_mut<'a>(
    draft: &'a mut Draft,
    child: Option<&str>,
    name: &str,
) -> Result<&'a mut FieldSpec, OpError> {
    let path = field_path(draft, child, name);
    let definition = definition_for(draft, child)?;
    definition
        .fields
        .iter_mut()
        .find(|f| f.name == name)
        .ok_or(OpError::UnknownField { path })
}

/// A mutable layout on the root or a child.
pub(crate) fn layout_mut<'a>(
    draft: &'a mut Draft,
    child: Option<&str>,
) -> Result<&'a mut FormLayout, OpError> {
    match child {
        None => Ok(&mut draft.form_layout),
        Some(child) => {
            let name = child.to_owned();
            draft
                .child_mut(&name)
                .map(|c| &mut c.form_layout)
                .ok_or(OpError::UnknownChild { name })
        }
    }
}

/// The definition of the root or a child.
pub(crate) fn definition_for<'a>(
    draft: &'a mut Draft,
    child: Option<&str>,
) -> Result<&'a mut Definition, OpError> {
    match child {
        None => Ok(&mut draft.doctype_def),
        Some(child) => {
            let name = child.to_owned();
            draft
                .child_mut(&name)
                .map(|c| &mut c.doctype_def)
                .ok_or(OpError::UnknownChild { name })
        }
    }
}

/// Checks a name a new field or DocType will take.
pub(crate) fn check_name(name: &str, reserved: bool) -> Result<(), OpError> {
    if !is_storage_name(name) {
        return Err(OpError::InvalidName {
            name: name.to_owned(),
        });
    }
    if reserved && is_reserved_field(name) {
        return Err(OpError::ReservedName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Refuses a value that looks like script or raw SQL.
pub(crate) fn check_value_text(text: &str, path: &str) -> Result<(), OpError> {
    if looks_like_script(text) {
        return Err(OpError::ScriptNotAllowed {
            path: path.to_owned(),
        });
    }
    if looks_like_sql(text) {
        return Err(OpError::RawSqlNotAllowed {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// Sets a select field's values.
pub(crate) fn set_options(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    options: &[String],
) -> Result<(), OpError> {
    let path = field_path(draft, child, name);
    let ty = field_mut(draft, child, name)?.ty;
    if ty != FieldType::Select {
        return Err(OpError::Unsupported {
            what: format!("options belong to a select field, not a {ty:?} field"),
        });
    }
    let mut seen: Vec<&str> = Vec::new();
    for option in options {
        if option.trim().is_empty() {
            return Err(OpError::BadOptions {
                path: path.clone(),
                why: "an option is empty".to_owned(),
            });
        }
        if seen.contains(&option.as_str()) {
            return Err(OpError::BadOptions {
                path: path.clone(),
                why: format!("the option `{option}` is repeated"),
            });
        }
        check_value_text(option, &path)?;
        seen.push(option.as_str());
    }
    field_mut(draft, child, name)?.options = Some(options.join("\n"));
    Ok(())
}

/// Sets a field's depends-on rule from a friendly condition.
pub(crate) fn set_depends_on(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    condition: Option<&Condition>,
) -> Result<(), OpError> {
    let path = field_path(draft, child, name);
    let text = match condition {
        None => None,
        Some(condition) => Some(condition.compile().map_err(|e| OpError::BadCondition {
            path: path.clone(),
            message: e.message,
        })?),
    };
    if let Some(text) = &text {
        check_value_text(text, &path)?;
    }
    field_mut(draft, child, name)?.depends_on = text;
    Ok(())
}

/// Adds a field to a definition.
pub(crate) fn add_field(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    label: Option<String>,
    ty: FieldType,
    insert_after: Option<&str>,
) -> Result<(), OpError> {
    check_name(name, true)?;
    if ty.is_table() {
        return Err(OpError::Unsupported {
            what: format!("a `{ty:?}` field must be added with the child table it points at"),
        });
    }
    let path = field_path(draft, child, name);
    let names = draft.field_names(child);
    if names.iter().any(|n| n == name) {
        return Err(OpError::DuplicateField { path });
    }
    if names.len() >= MAX_FIELDS {
        return Err(OpError::TooManyFields { limit: MAX_FIELDS });
    }
    let definition = definition_for(draft, child)?;
    let at = insert_position(definition, insert_after)?;
    let mut spec = FieldSpec::new(name, ty);
    spec.label = Some(label.unwrap_or_else(|| spot_views::humanize_label(name)));
    definition.fields.insert(at, spec);
    Ok(())
}

/// The index to insert a field at, after `insert_after`.
pub(crate) fn insert_position(
    definition: &Definition,
    insert_after: Option<&str>,
) -> Result<usize, OpError> {
    match insert_after {
        None => Ok(definition.fields.len()),
        Some(after) => match definition.fields.iter().position(|f| f.name == after) {
            Some(at) => Ok(at + 1),
            None => Err(OpError::UnknownField {
                path: format!("{}.{}", definition.name, after),
            }),
        },
    }
}

/// Renames a field and its layout item.
pub(crate) fn rename_field(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    to: &str,
) -> Result<(), OpError> {
    check_name(to, true)?;
    let path = field_path(draft, child, name);
    let names = draft.field_names(child);
    if names.iter().any(|n| n == to) {
        return Err(OpError::DuplicateField {
            path: field_path(draft, child, to),
        });
    }
    let definition = definition_for(draft, child)?;
    let field = definition
        .fields
        .iter_mut()
        .find(|f| f.name == name)
        .ok_or(OpError::UnknownField { path })?;
    field.name = to.to_owned();
    if let Some(layout) = layout_for(draft, child) {
        layout.rename_field(name, to);
    }
    Ok(())
}

/// Removes a field, its layout item and the references that named it.
pub(crate) fn remove_field(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    confirm: Option<&str>,
) -> Result<(), OpError> {
    let path = field_path(draft, child, name);
    let target_name = child.unwrap_or_else(|| draft.doctype());
    let expected = data_loss_token(target_name, name);
    if confirm != Some(expected.as_str()) {
        return Err(OpError::NeedsDataLossConfirmation { path });
    }
    let definition = definition_for(draft, child)?;
    if !definition.fields.iter().any(|f| f.name == name) {
        return Err(OpError::UnknownField { path });
    }
    if field_is_referenced(definition, name) {
        return Err(OpError::Referenced {
            name: name.to_owned(),
        });
    }
    definition.fields.retain(|f| f.name != name);
    if let Some(layout) = layout_for(draft, child) {
        layout.remove_field(name);
    }
    Ok(())
}

/// Whether another field still refers to `name`.
pub(crate) fn field_is_referenced(definition: &Definition, name: &str) -> bool {
    definition.fields.iter().any(|f| {
        let fetch = f
            .fetch_from
            .as_deref()
            .and_then(|v| v.split_once('.'))
            .is_some_and(|(link, target)| link == name || target == name);
        fetch || f.currency_field.as_deref() == Some(name)
    })
}

/// Changes a field's type after the compatibility check.
pub(crate) fn change_type(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    ty: FieldType,
    stats: &SampleStats,
) -> Result<(), OpError> {
    let path = field_path(draft, child, name);
    let current = field_mut(draft, child, name)?.ty;
    let plan = crate::typechange::classify(current, ty, stats);
    if plan.classification == Classification::Refused {
        return Err(OpError::TypeChangeRefused {
            path,
            reason: plan.summary,
        });
    }
    apply_type(field_mut(draft, child, name)?, ty);
    Ok(())
}

/// Applies a type, clearing the properties that only fit the old type.
pub(crate) fn apply_type(field: &mut FieldSpec, ty: FieldType) {
    field.ty = ty;
    if !matches!(
        ty,
        FieldType::Select | FieldType::Link | FieldType::DynamicLink
    ) {
        field.options = None;
    }
    if ty != FieldType::Decimal {
        field.scale = None;
        field.precision = None;
    }
    if ty != FieldType::Text {
        field.max_chars = None;
    }
}

/// Reorders a field within its definition's field list.
pub(crate) fn reorder_field(
    draft: &mut Draft,
    child: Option<&str>,
    name: &str,
    after: Option<&str>,
) -> Result<(), OpError> {
    let path = field_path(draft, child, name);
    let definition = definition_for(draft, child)?;
    let at = definition
        .fields
        .iter()
        .position(|f| f.name == name)
        .ok_or(OpError::UnknownField { path })?;
    let field = definition.fields.remove(at);
    let insert = insert_position(definition, after)?;
    definition
        .fields
        .insert(insert.min(definition.fields.len()), field);
    Ok(())
}

/// Adds a child DocType and the table field pointing at it.
pub(crate) fn add_child_table(
    draft: &mut Draft,
    name: &str,
    label: Option<String>,
    child: &ChildDraft,
    insert_after: Option<&str>,
) -> Result<(), OpError> {
    check_name(name, true)?;
    check_name(&child.doctype_def.name, false)?;
    if is_protected_doctype(&child.doctype_def.name) {
        return Err(OpError::BadTarget {
            name: child.doctype_def.name.clone(),
        });
    }
    if draft.doctype_def.is_child {
        return Err(OpError::TooDeep);
    }
    if draft.children.len() >= MAX_FIELDS {
        return Err(OpError::TooManyFields { limit: MAX_FIELDS });
    }
    if draft.child(&child.doctype_def.name).is_some() {
        return Err(OpError::DuplicateField {
            path: child.doctype_def.name.clone(),
        });
    }
    if draft.field_names(None).iter().any(|n| n == name) {
        return Err(OpError::DuplicateField {
            path: format!("{}.{name}", draft.doctype()),
        });
    }
    let at = insert_position(&draft.doctype_def, insert_after)?;
    let mut spec = FieldSpec::new(name, FieldType::Table);
    spec.label = Some(label.unwrap_or_else(|| spot_views::humanize_label(name)));
    spec.options = Some(child.doctype_def.name.clone());
    draft.doctype_def.fields.insert(at, spec);
    let mut child = child.clone();
    child.doctype_def.is_child = true;
    draft.children.push(child);
    Ok(())
}

/// Adds a link field.
pub(crate) fn add_link(
    draft: &mut Draft,
    name: &str,
    label: Option<String>,
    target: &str,
    insert_after: Option<&str>,
) -> Result<(), OpError> {
    check_name(name, true)?;
    check_name(target, false)?;
    if is_protected_doctype(target) || draft.child(target).is_some() {
        return Err(OpError::BadTarget {
            name: target.to_owned(),
        });
    }
    let path = format!("{}.{name}", draft.doctype());
    if draft.field_names(None).iter().any(|n| n == name) {
        return Err(OpError::DuplicateField { path });
    }
    let at = insert_position(&draft.doctype_def, insert_after)?;
    let mut spec = FieldSpec::new(name, FieldType::Link);
    spec.label = Some(label.unwrap_or_else(|| spot_views::humanize_label(name)));
    spec.options = Some(target.to_owned());
    draft.doctype_def.fields.insert(at, spec);
    Ok(())
}

/// Checks a naming pattern: `hash`, `prompt`, `field:<name>` or a series with a run of `#`.
pub(crate) fn check_naming(pattern: &str) -> Result<(), OpError> {
    if naming_is_valid(pattern) {
        Ok(())
    } else {
        Err(OpError::BadNaming {
            naming: pattern.to_owned(),
        })
    }
}

/// Whether a naming pattern is one the platform accepts: `hash`, `prompt`, `field:<name>` or a
/// series containing a run of `#` and no whitespace.
#[must_use]
pub fn naming_is_valid(pattern: &str) -> bool {
    if pattern.trim().is_empty() || pattern.len() > 200 {
        return false;
    }
    if matches!(pattern, "hash" | "prompt") {
        return true;
    }
    if let Some(field) = pattern.strip_prefix("field:") {
        return is_storage_name(field);
    }
    pattern.contains('#') && !pattern.chars().any(char::is_whitespace)
}

/// The layout of the root or a child, when it exists.
pub(crate) fn layout_for<'a>(
    draft: &'a mut Draft,
    child: Option<&str>,
) -> Option<&'a mut FormLayout> {
    match child {
        None => Some(&mut draft.form_layout),
        Some(child) => draft.child_mut(child).map(|c| &mut c.form_layout),
    }
}
