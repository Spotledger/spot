//! Shared fixtures: a DocType set with a secret field, and helpers to build images and ids.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_activity::wire;
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

/// The DocType set: an invoice (posted, so engine fields are declared) with an items table,
/// and every scalar type the diff must handle. Secret masking is keyed on the field kind on
/// the app-API view; the v0 `FieldType` list has no secret variant, so the mask tests
/// (tests/diff.rs) build that view directly.
pub fn definitions() -> Vec<Definition> {
    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut number = required(field("number", FieldType::Text));
    number.max_chars = Some(16);
    let mut customer = required(field("customer", FieldType::Link));
    customer.options = Some("customer".into());
    let mut status = field("status", FieldType::Select);
    status.options = Some("Open\nPaid\nOverdue".into());
    let mut items = field("items", FieldType::Table);
    items.options = Some("sales_invoice_item".into());
    invoice.fields = vec![
        number,
        customer,
        status,
        field("posting_date", FieldType::Date),
        decimal("total", 2),
        field("remarks", FieldType::LongText),
        items,
        // The engine adds doc_state and amended_from to a posted DocType's view; the raw def
        // does not carry them (the registry does), so nothing to add here.
    ];

    let mut item = Definition::new("sales_invoice_item");
    item.is_child = true;
    let mut label = required(field("item", FieldType::Text));
    label.max_chars = Some(10);
    item.fields = vec![label, required(decimal("qty", 3))];

    let mut customer_doc = Definition::new("customer");
    customer_doc.fields = vec![required(field("name", FieldType::Text))];

    vec![customer_doc, invoice, item]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// The base instant every test anchors to (a fixed business time, never a clock).
pub const NOW: i64 = 1_790_000_000;

pub fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(NOW + secs).unwrap()
}

/// A row with an explicit id, so the diff can name it.
pub fn row_with(id: RecordId, item: &str, quantity: Value) -> ChildRow {
    let mut r = ChildRow {
        id,
        fields: Default::default(),
    };
    r.fields.insert(n("item"), item.into());
    r.fields.insert(n("qty"), quantity);
    r
}

/// A valid invoice image. The customer link is a fixed id, so two calls give two images that
/// differ only where the test changes them.
pub fn invoice_fields() -> std::collections::BTreeMap<Name, Value> {
    let mut f = std::collections::BTreeMap::new();
    f.insert(n("number"), Value::Text("SI-0001".into()));
    f.insert(n("customer"), Value::Id(uuid7(8, 8)));
    f.insert(n("status"), Value::Text("Open".into()));
    f.insert(
        n("posting_date"),
        Value::Date(time::Date::from_calendar_date(2026, time::Month::October, 6).unwrap()),
    );
    f.insert(n("total"), Value::Decimal(Decimal::new(10_000, 2)));
    f
}

pub fn invoice_tables() -> std::collections::BTreeMap<Name, Vec<ChildRow>> {
    std::collections::BTreeMap::new()
}

/// Two distinct ids, fixed, so tests can name them.
pub fn ids() -> (RecordId, RecordId) {
    let a = uuid7(1, 1);
    let b = uuid7(1, 2);
    (a, b)
}

/// A deterministic UUIDv7 (the storage contract requires the version bit): counters `seq`
/// (0..=0xfff) make distinct ids test-side.
pub fn uuid7(site: u32, seq: u32) -> RecordId {
    let unix_ms: i64 = 1_790_000_000 * 1000 + i64::from(site) * 0x1000 + i64::from(seq);
    let mut b = [0u8; 16];
    b[0] = (unix_ms >> 40) as u8;
    b[1] = (unix_ms >> 32) as u8;
    b[2] = (unix_ms >> 24) as u8;
    b[3] = (unix_ms >> 16) as u8;
    b[4] = (unix_ms >> 8) as u8;
    b[5] = unix_ms as u8;
    // version 7
    b[6] = 0x70 | ((seq >> 8) as u8 & 0x0f);
    b[7] = seq as u8;
    b[8] = 0x80 | (site as u8 & 0x3f); // variant 10
    let tail_a = site.to_be_bytes();
    let tail_b = seq.to_be_bytes();
    b[9] = tail_a[3];
    b[10] = tail_b[2];
    b[11] = tail_b[3];
    b[12] = tail_b[0];
    b[13] = tail_b[1];
    b[14] = tail_b[2];
    b[15] = tail_b[3];
    RecordId::from_bytes(b).ok().unwrap_or(RecordId::new_v7())
}

/// The value JSON of a row (for `is_row_change` expectations in tests).
pub fn row_json(r: &ChildRow) -> String {
    wire::row::to_json(r).to_string()
}

/// A decimal from a string, exact.
pub fn dec(text: &str) -> Value {
    Value::Decimal(text.parse::<Decimal>().unwrap())
}
