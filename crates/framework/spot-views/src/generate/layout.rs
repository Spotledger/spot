//! The form shape: how the generated form groups the effective fields (brief
//! deliverable 1). Sections are built by [`sections_of`]; the shape callers ask for is
//! [`form_view`].

use spot_meta::{Definition, FieldSpec, FieldType};

use super::humanize::humanize_label;
use super::{
    CUSTOM_PREFIX, CUSTOM_SECTION, CUSTOM_SECTION_TITLE, ENGINE_FIELDS, FIELDS_SECTION,
    STATUS_SECTION, STATUS_SECTION_TITLE, TABLE_SECTION_PREFIX,
};
use crate::viewdef::{Item, Section, ViewDef, ViewKind};

/// A field item of the generated view: the field's storage name, the definition's label
/// when it has one (the label fallback humanises the name instead: [`humanize_label`]).
fn field_item(field: &FieldSpec) -> Item {
    Item::Field {
        field: field.name.clone(),
        label: field.label.clone(),
        control: None,
        lookup: None,
    }
}

/// An unbound control item.
fn control_item(control: &str) -> Item {
    Item::Control {
        control: control.to_owned(),
        text: None,
    }
}

/// The `table_<field>` section of one child-table field: titled by the field's label
/// (humanised from the storage name when it has none), `columns = 1` so the child grid
/// renders full width.
fn table_section(field: &FieldSpec) -> Section {
    Section {
        id: format!("{TABLE_SECTION_PREFIX}{}", field.name),
        title: Some(field_label(field)),
        columns: Some(1),
        items: vec![field_item(field)],
    }
}

/// The collapsed Status section: its items, then the `divider` unbound marker as the
/// visual fold point. (The model's unbound set — `heading|divider|spacer|help` — has no
/// `collapsed` kind: whether the Status section opens folded is the renderer's read of
/// the section's *Status* title + read-only contents, never a layout fact in the TOML.
/// The divider keeps the closed-off shape explicit in every golden file.)
fn collapsed_section(id: &str, title: &str, items: Vec<Item>) -> Section {
    let mut items = items;
    items.push(control_item("divider"));
    Section {
        id: id.to_owned(),
        title: Some(title.to_owned()),
        columns: None,
        items,
    }
}

/// A field's label, humanised from the storage name when the definition has none (the
/// label fallback of the generated views, brief deliverable 4).
pub(super) fn field_label(field: &FieldSpec) -> String {
    field
        .label
        .clone()
        .unwrap_or_else(|| humanize_label(field.name.as_str()))
}

/// Which bucket of the generated form a field belongs to.
enum Placement {
    /// The scalar `fields` section, in definition order.
    Fields,
    /// A `table_<field>` section of its own (titled by the field's label).
    Table,
    /// After the scalar fields: `long_text` and `json`.
    LongForm,
    /// The collapsed `status` section: read-only and engine-managed fields.
    Status,
    /// The `custom` section: the site's `custom_` fields.
    Custom,
}

/// A field's placement by its properties, in the order the rules decide it: a custom
/// field is a *provenance* fact (it goes to `custom` wherever it sits in the definition),
/// the read-only and engine-managed fields are a *state* fact (the collapsed `status`
/// section), and the type decides between scalar, long-form and table.
fn placement(field: &FieldSpec) -> Placement {
    if field.name.starts_with(CUSTOM_PREFIX) {
        return Placement::Custom;
    }
    if field.read_only || ENGINE_FIELDS.contains(&field.name.as_str()) {
        return Placement::Status;
    }
    match field.ty {
        FieldType::Table | FieldType::TableMultiselect => Placement::Table,
        FieldType::LongText | FieldType::Json => Placement::LongForm,
        FieldType::Bool
        | FieldType::Int
        | FieldType::Decimal
        | FieldType::Text
        | FieldType::Select
        | FieldType::Date
        | FieldType::Time
        | FieldType::DateTime
        | FieldType::Link
        | FieldType::DynamicLink
        | FieldType::Bytes
        | FieldType::Geo
        | FieldType::Range
        | FieldType::Attach
        | FieldType::Secret => Placement::Fields,
    }
}

/// The default form of `definition` (brief deliverable 1): top-level sections, no tabs —
/// a runtime DocType needs the useful two-pane scroll, not tab hunting. The scalar fields
/// carry the definition order; the long-form fields come after them; the `status` section
/// is collapsed; the `custom` section sits last, so a site's additions are easy to find at
/// the same place on every form.
pub(super) fn form_view(definition: &Definition) -> ViewDef {
    ViewDef {
        doctype: definition.name.clone(),
        kind: ViewKind::Form,
        role: None,
        title: definition.label.clone(),
        density: None,
        tabs: Vec::new(),
        sections: sections_of(definition),
        columns: Vec::new(),
        sort: None,
        actions: Vec::new(),
    }
}

/// The form's sections in order: `fields`, one `table_<field>` section per table field
/// (its own section, titled by the field's label), a `long_form` section when the DocType
/// has `long_text`/`json` fields, the collapsed `status` section when there are read-only
/// or engine-managed fields, and a `custom` section when the site has added fields. An
/// empty DocType still gets the `fields` section: a form needs tabs or top-level sections
/// (the model's own shape rule).
pub(super) fn sections_of(definition: &Definition) -> Vec<Section> {
    let mut fields: Vec<Item> = Vec::new();
    let mut tables: Vec<&FieldSpec> = Vec::new();
    let mut long_form: Vec<Item> = Vec::new();
    let mut status: Vec<Item> = Vec::new();
    let mut custom: Vec<Item> = Vec::new();

    for field in &definition.fields {
        let item = field_item(field);
        match placement(field) {
            Placement::Fields => fields.push(item),
            Placement::Table => tables.push(field),
            Placement::LongForm => long_form.push(item),
            Placement::Status => status.push(item),
            Placement::Custom => custom.push(item),
        }
    }

    let mut sections = Vec::new();
    // The scalar `fields` section: always present, never titled (the DocType's frame
    // already titling the form would repeat it).
    sections.push(Section {
        id: FIELDS_SECTION.to_owned(),
        title: None,
        columns: None,
        items: fields,
    });
    for field in tables {
        sections.push(table_section(field));
    }
    if !long_form.is_empty() {
        sections.push(Section {
            id: "long_form".to_owned(),
            title: Some("Details".to_owned()),
            columns: None,
            items: long_form,
        });
    }
    if !status.is_empty() {
        sections.push(collapsed_section(
            STATUS_SECTION,
            STATUS_SECTION_TITLE,
            status,
        ));
    }
    if !custom.is_empty() {
        sections.push(Section {
            id: CUSTOM_SECTION.to_owned(),
            title: Some(CUSTOM_SECTION_TITLE.to_owned()),
            columns: None,
            items: custom,
        });
    }
    sections
}
