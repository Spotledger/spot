//! Shared fixtures: an invoice with two tables of different row DocTypes, and a product with a
//! `table_multiselect`.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::prelude::{Decimal, Name, RecordId, Value};
use spot_app_api::{ChildRow, Document};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn decimal(name: &str, scale: u8) -> FieldSpec {
    let mut f = field(name, FieldType::Decimal);
    f.scale = Some(scale);
    f
}

fn table(name: &str, child: &str, ty: FieldType) -> FieldSpec {
    let mut f = field(name, ty);
    f.options = Some(child.to_owned());
    f
}

fn link(name: &str, target: &str) -> FieldSpec {
    let mut f = field(name, FieldType::Link);
    f.options = Some(target.to_owned());
    f
}

fn child(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = Definition::new(name);
    d.is_child = true;
    d.fields = fields;
    d
}

pub fn meta() -> Arc<EffectiveMeta> {
    let mut invoice = Definition::new("sales_invoice");
    invoice.fields = vec![
        field("number", FieldType::Text),
        table("items", "sales_invoice_item", FieldType::Table),
        table("taxes", "sales_invoice_tax", FieldType::Table),
    ];
    let item = child(
        "sales_invoice_item",
        vec![
            field("item", FieldType::Text),
            decimal("qty", 3),
            decimal("rate", 2),
        ],
    );
    let tax = child(
        "sales_invoice_tax",
        vec![
            field("description", FieldType::Text),
            decimal("tax_rate", 2),
        ],
    );
    let mut tag = Definition::new("tag");
    tag.fields = vec![field("label", FieldType::Text)];
    let mut product = Definition::new("product");
    product.fields = vec![
        field("name", FieldType::Text),
        table("tags", "product_tag", FieldType::TableMultiselect),
        table("notes", "product_note", FieldType::Table),
    ];
    let product_tag = child("product_tag", vec![link("tag", "tag")]);
    let note = child("product_note", vec![field("text", FieldType::Text)]);
    Registry::new(vec![invoice, item, tax, tag, product, product_tag, note])
        .unwrap()
        .effective()
}

/// A valid record id from a number: a UUIDv7 whose last bytes are `i`, so ids sort by `i`.
pub fn id(i: u32) -> RecordId {
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&[0x01, 0x90, 0x00, 0x00, 0x00, 0x00]);
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[12..].copy_from_slice(&i.to_be_bytes());
    RecordId::from_bytes(bytes).unwrap()
}

pub fn qty(milli: i64) -> Value {
    Value::Decimal(Decimal::new(milli, 3))
}

/// A row of `items`.
pub fn item(i: u32, name: &str, q: i64) -> ChildRow {
    let mut fields = BTreeMap::new();
    fields.insert(n("item"), Value::Text(name.to_owned()));
    fields.insert(n("qty"), qty(q));
    ChildRow { id: id(i), fields }
}

/// An invoice (id 1000) with the given rows in `items`, as loaded from storage.
pub fn loaded(items: &[ChildRow]) -> Document {
    let mut tables = BTreeMap::new();
    tables.insert(n("items"), items.to_vec());
    Document::loaded(
        n("sales_invoice"),
        id(1000),
        spot_app_api::prelude::Version::FIRST,
        None,
        BTreeMap::new(),
        tables,
    )
}

pub fn problems(e: &spot_app_api::AppError) -> Vec<(String, String)> {
    e.fields
        .iter()
        .map(|f| (f.field.as_str().to_owned(), f.code.clone()))
        .collect()
}
