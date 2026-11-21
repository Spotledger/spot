// A test DocType set over the spot-import engine: customers and posted sales invoices with
// item rows, the layout the brief's examples describe (WP-DOC-09.T1).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

/// `Name::new` for tests (every name here is a valid one).
pub fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

pub fn required(mut f: FieldSpec) -> FieldSpec {
    f.required = true;
    f
}

pub fn decimal(name: &str, scale: u8) -> FieldSpec {
    let mut f = field(name, FieldType::Decimal);
    f.scale = Some(scale);
    f
}

/// The same DocType set as spot_document's test common module: a customer, a posted sales
/// invoice with a table, and the invoice's item child DocType.
pub fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    let mut name = required(field("name", FieldType::Text));
    name.max_chars = Some(20);
    let mut rating = field("rating", FieldType::Select);
    rating.options = Some("A\nB\nC".into());
    customer.fields = vec![name, rating];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut number = required(field("number", FieldType::Text));
    number.max_chars = Some(16);
    let mut customer_link = required(field("customer", FieldType::Link));
    customer_link.options = Some("customer".into());
    let mut status = field("status", FieldType::Select);
    status.options = Some("Open\nPaid\nOverdue".into());
    let mut items = field("items", FieldType::Table);
    items.options = Some("sales_invoice_item".into());
    let mut total = decimal("total", 2);
    total.precision = Some(10);
    invoice.fields = vec![
        number,
        customer_link,
        status,
        {
            let mut due = field("due_date", FieldType::Date);
            due.label = Some("Due Date".into());
            due
        },
        field("posted_at", FieldType::DateTime),
        total,
        field("discount_days", FieldType::Int),
        field("urgent", FieldType::Bool),
        field("remarks", FieldType::LongText),
        items,
    ];

    let mut item = Definition::new("sales_invoice_item");
    item.is_child = true;
    let mut item_name = required(field("item", FieldType::Text));
    item_name.max_chars = Some(10);
    let mut kind = field("kind", FieldType::Select);
    kind.options = Some("Goods\nService".into());
    item.fields = vec![item_name, required(decimal("qty", 3)), kind];

    vec![customer, invoice, item]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}
