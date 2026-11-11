//! The list and grid shapes: the list's columns and default sort (brief deliverable 2),
//! and the child-table grid (brief deliverable 3).

use spot_meta::{Definition, FieldSpec, FieldType};

use super::LIST_EXTRA_COLUMNS;
use crate::viewdef::{Column, Sort, ViewDef, ViewKind};

/// A column for the field's storage name (the definition's label shows through; the
/// per-view `label` override is for hand-written views).
fn column_named(name: &str) -> Column {
    Column {
        field: name.to_owned(),
        label: None,
    }
}

/// The list is not for every type: the long-form and child-table types never show as
/// columns (they are documents of their own, not a glanceable value).
fn fits_the_list(ty: FieldType) -> bool {
    !matches!(
        ty,
        FieldType::LongText
            | FieldType::Json
            | FieldType::Bytes
            | FieldType::Geo
            | FieldType::Range
            | FieldType::Table
    )
}

/// The column priority (brief deliverable 2): link fields first (who and what a row talks
/// about), then selects (state at a glance), then date and datetime (when), then decimal
/// and int (how much), then everything else that fits. A `table_multiselect` is excluded
/// with the long-form types (its value is a set of rows, not a column).
fn priority(ty: FieldType) -> u8 {
    match ty {
        FieldType::Link | FieldType::DynamicLink => 0,
        FieldType::Select => 1,
        FieldType::Date | FieldType::DateTime => 2,
        FieldType::Decimal | FieldType::Int => 3,
        FieldType::Bool
        | FieldType::Time
        | FieldType::Text
        | FieldType::Attach
        | FieldType::Secret => 4,
        FieldType::TableMultiselect => u8::MAX,
        // `fits_the_list` already kept the other long-form and table types out.
        FieldType::LongText
        | FieldType::Json
        | FieldType::Bytes
        | FieldType::Geo
        | FieldType::Range
        | FieldType::Table => u8::MAX,
    }
}

/// The default list of `definition` (brief deliverable 2): the `title_field` first, then
/// up to [`LIST_EXTRA_COLUMNS`] more fields — priority class first, definition order
/// within a class; the long-form and table types never show. The field-level
/// `in_list_view` hint is what a hand-written list honours; the *default* list reads the
/// definition's type priority instead, because a runtime DocType has not earned the hint
/// yet and a default layout must not depend on hints a runtime creator never set.
pub(super) fn list_view(definition: &Definition) -> ViewDef {
    ViewDef {
        doctype: definition.name.clone(),
        kind: ViewKind::List,
        role: None,
        title: definition.label.clone(),
        density: None,
        tabs: Vec::new(),
        sections: Vec::new(),
        columns: list_columns(definition),
        sort: default_sort(definition),
        actions: Vec::new(),
    }
}

/// The list columns: the `title_field` first (when the DocType declares one and the field
/// exists and fits the list), then up to [`LIST_EXTRA_COLUMNS`] more, by priority class
/// then definition order.
pub(super) fn list_columns(definition: &Definition) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    if let Some(title_field) = definition
        .title_field
        .as_deref()
        .filter(|name| fit_for_list(definition, name))
    {
        columns.push(column_named(title_field));
        taken.push(title_field.to_owned());
    }
    let mut ranked: Vec<(u8, usize, &FieldSpec)> = definition
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| fits_the_list(field.ty) && !taken.contains(&field.name))
        .map(|(position, field)| (priority(field.ty), position, field))
        .collect();
    ranked.sort_by_key(|(class, position, _)| (*class, *position));
    for (_, _, field) in ranked.into_iter().take(LIST_EXTRA_COLUMNS) {
        columns.push(column_named(field.name.as_str()));
    }
    columns
}

/// Whether the named field exists and fits the list (a title field that is a `long_text`
/// stays off the columns).
fn fit_for_list(definition: &Definition, name: &str) -> bool {
    definition
        .fields
        .iter()
        .find(|field| field.name == name)
        .is_some_and(|field| fits_the_list(field.ty))
}

/// The default sort (brief deliverable 2): the first datetime or date field by definition
/// order, descending (the newest document on top); else the title field, ascending; else
/// (a DocType with neither) the first field, ascending. Deterministic in every case — a
/// generated list is sortable, never sortless.
pub(super) fn default_sort(definition: &Definition) -> Option<Sort> {
    let instant = definition
        .fields
        .iter()
        .find(|field| matches!(field.ty, FieldType::DateTime | FieldType::Date));
    if let Some(field) = instant {
        return Some(Sort {
            field: field.name.clone(),
            descending: true,
        });
    }
    let titled = definition
        .title_field
        .as_deref()
        .and_then(|name| definition.fields.iter().find(|field| field.name == name));
    if let Some(field) = titled.or(definition.fields.first()) {
        return Some(Sort {
            field: field.name.clone(),
            descending: false,
        });
    }
    None
}

/// The default grid of a child `definition` (brief deliverable 3): every field of the
/// child in definition order, with decimal and int fields hinted right-aligned (money and
/// quantities read aligned), through the item's `control` presentation hint. A child
/// DocType renders its grid as the columns of the parent's table item, so there are no
/// tabs and no sections on a grid.
pub(super) fn grid_view(definition: &Definition) -> ViewDef {
    ViewDef {
        doctype: definition.name.clone(),
        kind: ViewKind::Grid,
        role: None,
        title: definition.label.clone(),
        density: None,
        tabs: Vec::new(),
        sections: Vec::new(),
        columns: grid_columns(definition),
        sort: None,
        actions: Vec::new(),
    }
}

/// The grid columns: every field of the child in order, decimal and int hinted
/// right-aligned.
fn grid_columns(definition: &Definition) -> Vec<Column> {
    definition
        .fields
        .iter()
        .map(|field| Column {
            field: field.name.clone(),
            label: None,
        })
        .collect()
}
