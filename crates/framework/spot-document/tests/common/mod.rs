//! A small DocType set for the tests: customers, posted sales invoices with item rows.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Decimal, Name, OffsetDateTime, RecordId, Value};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn required(mut f: FieldSpec) -> FieldSpec {
    f.required = true;
    f
}

pub fn decimal(name: &str, scale: u8) -> FieldSpec {
    let mut f = field(name, FieldType::Decimal);
    f.scale = Some(scale);
    f
}

pub fn with_default(mut f: FieldSpec, default: &str) -> FieldSpec {
    f.default = Some(default.to_owned());
    f
}

pub fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    let mut name = required(field("name", FieldType::Text));
    name.max_chars = Some(20);
    customer.fields = vec![name];

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
        field("posting_date", FieldType::Date),
        field("created_at", FieldType::DateTime),
        total,
        with_default(field("discount_days", FieldType::Int), "0"),
        with_default(field("urgent", FieldType::Bool), "false"),
        field("remarks", FieldType::LongText),
        field("attachment", FieldType::Bytes),
        field("extra", FieldType::Json),
        items,
    ];

    let mut item = Definition::new("sales_invoice_item");
    item.is_child = true;
    let mut name = required(field("item", FieldType::Text));
    name.max_chars = Some(10);
    let mut kind = field("kind", FieldType::Select);
    kind.options = Some("Goods\nService".into());
    item.fields = vec![
        name,
        required(decimal("qty", 3)),
        with_default(decimal("rate", 2), "0.00"),
        kind,
    ];
    vec![customer, invoice, item]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

pub fn money(cents: i64) -> Value {
    Value::Decimal(Decimal::new(cents, 2))
}

pub fn qty(milli: i64) -> Value {
    Value::Decimal(Decimal::new(milli, 3))
}

pub fn invoice() -> Document {
    Document::new(n("sales_invoice"), RecordId::new_v7())
}

pub fn row(item: &str, quantity: Value) -> ChildRow {
    let mut r = ChildRow {
        id: RecordId::new_v7(),
        fields: Default::default(),
    };
    r.fields.insert(n("item"), item.into());
    r.fields.insert(n("qty"), quantity);
    r
}

/// A valid invoice with two rows.
pub fn valid_invoice() -> Document {
    let mut doc = invoice();
    doc.set(n("number"), "SI-0001");
    doc.set(n("customer"), RecordId::new_v7());
    doc.rows_mut(n("items")).push(row("bolt", qty(2_000)));
    doc.rows_mut(n("items")).push(row("nut", qty(1_500)));
    doc
}

/// `(path, code)` of every field error, in order.
pub fn problems(e: &spot_app_api::AppError) -> Vec<(String, String)> {
    e.fields
        .iter()
        .map(|f| (f.field.to_string(), f.code.clone()))
        .collect()
}
