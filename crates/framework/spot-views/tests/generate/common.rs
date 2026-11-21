//! Shared fixtures for the generate tests: three sample DocTypes built through
//! `spot_meta::Registry` exactly as the platform builds them (brief deliverable 5).
//!
//! Every link target and child table the fixtures name is a DocType the set defines,
//! every decimal names its scale and the title fields are text — the same rules
//! `Registry::build` enforces on a site's metadata.

#![allow(dead_code, missing_docs)]

use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

/// The scale a fixtures decimal carries.
const SCALE: u8 = 2;

/// A text field with a label.
fn labelled_text(name: &str, label: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Text);
    field.label = Some(label.to_owned());
    field
}

/// A plain text field used by the shapes tests.
pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// A link field to `target`.
pub fn link(name: &str, target: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(target.to_owned());
    field
}

/// A decimal field with the fixtures' scale.
pub fn decimal(name: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Decimal);
    field.scale = Some(SCALE);
    field
}

/// `item`: a plain master DocType — a named code, a link, a select, numbers and long
/// text notes (the golden form's scalar/long-form split).
pub fn item() -> Definition {
    let mut definition = Definition::new("item");
    definition.label = Some("Item".to_owned());
    definition.title_field = Some("item_code".to_owned());
    definition.fields = vec![
        labelled_text("item_code", "Item Code"),
        link("item_group", "item_group"),
        {
            let mut status = FieldSpec::new("status", FieldType::Select);
            status.label = Some("Status".to_owned());
            status.options = Some("active\ninactive".to_owned());
            status
        },
        decimal("opening_balance"),
        FieldSpec::new("reorder_level", FieldType::Int),
        FieldSpec::new("description", FieldType::LongText),
        FieldSpec::new("notes", FieldType::LongText),
    ];
    definition
}

/// `item_group`: the master `item`'s link target; a plain master whose fields carry no
/// labels — every generated label comes from the
/// [`humanize_label`](spot_views::humanize_label) fallback.
pub fn item_group() -> Definition {
    let mut definition = Definition::new("item_group");
    definition.title_field = Some("group_name".to_owned());
    definition.fields = vec![text("group_name"), text("vat_note2")];
    definition
}

/// `customer`: the invoice's link target.
pub fn customer() -> Definition {
    let mut definition = Definition::new("customer");
    definition.label = Some("Customer".to_owned());
    definition.title_field = Some("customer_name".to_owned());
    definition.fields = vec![labelled_text("customer_name", "Customer Name")];
    definition
}

/// `sales_invoice`: a posted document — a customer name, dates, scaled money, a
/// `long_text` remark and a child table `sales_invoice_item` (the brief's "posted
/// document with a child table").
pub fn sales_invoice() -> Definition {
    let mut definition = Definition::new("sales_invoice");
    definition.label = Some("Sales Invoice".to_owned());
    definition.is_posted = true;
    definition.title_field = Some("customer".to_owned());
    definition.fields = vec![
        labelled_text("customer", "Customer"),
        FieldSpec::new("posting_date", FieldType::Date),
        FieldSpec::new("due_date", FieldType::Date),
        decimal("grand_total"),
        FieldSpec::new("remarks", FieldType::LongText),
        {
            let mut items = FieldSpec::new("items", FieldType::Table);
            items.label = Some("Invoice Lines".to_owned());
            items.options = Some("sales_invoice_item".to_owned());
            items
        },
    ];
    definition
}

/// `sales_invoice_item`: the child of `sales_invoice.items` — the grid DocType.
pub fn sales_invoice_item() -> Definition {
    let mut definition = Definition::new("sales_invoice_item");
    definition.label = Some("Invoice Line".to_owned());
    definition.is_child = true;
    definition.fields = vec![
        link("item_code", "item"),
        decimal("qty"),
        decimal("rate"),
        decimal("amount"),
        text("note"),
    ];
    definition
}

/// The three sample DocTypes the golden files pin (brief deliverable 5): the plain
/// master `item`, the posted document `sales_invoice` with its child, and the child
/// DocType `sales_invoice_item` itself — plus the link targets the three need
/// (`item_group`, `customer`).
pub fn golden_fixtures() -> Vec<Definition> {
    vec![
        item(),
        item_group(),
        customer(),
        sales_invoice(),
        sales_invoice_item(),
    ]
}

/// Builds the effective metadata over `definitions`, through `Registry::build` as the
/// platform does.
///
/// # Panics
/// The fixtures are checked-in and valid; a failure is a setup bug.
pub fn effective_of(definitions: Vec<Definition>) -> EffectiveMeta {
    Registry::build(definitions, &Customisations::new())
        .unwrap_or_else(|errors| panic!("fixtures do not build: {errors:?}"))
}

/// The golden fixtures' effective metadata (posted DocTypes get their engine fields).
///
/// # Panics
/// As [`effective_of`].
pub fn effective() -> EffectiveMeta {
    effective_of(golden_fixtures())
}

/// A live `Registry` over the fixtures, for the custom-field tests.
///
/// # Panics
/// As [`effective_of`].
pub fn registry_fixtures() -> Registry {
    Registry::new(golden_fixtures())
        .unwrap_or_else(|errors| panic!("fixtures do not build: {errors:?}"))
}
