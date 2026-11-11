//! Shared fixtures for the `spot-rule-graph` tests: the metadata of each corpus, and
//! deterministic record keys.
#![allow(dead_code)]

use std::sync::Arc;

use rust_decimal::Decimal;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_storage_contract::{Name, NodeRef, RecordId, Value};

/// A field with an optional `options` (a link's target DocType).
fn field(name: &str, ty: FieldType, options: Option<&str>) -> FieldSpec {
    let mut spec = FieldSpec::new(name, ty);
    spec.options = options.map(str::to_owned);
    if ty == FieldType::Decimal {
        spec.scale = Some(2);
    }
    spec
}

/// The effective metadata of a corpus fixture.
pub fn meta(name: &str) -> Arc<EffectiveMeta> {
    let definitions = match name {
        "accounts" => {
            let mut account = Definition::new("account");
            account
                .fields
                .push(field("parent_account", FieldType::Link, Some("account")));
            account
                .fields
                .push(field("amount", FieldType::Decimal, None));
            account.fields.push(field("code", FieldType::Text, None));
            account
                .fields
                .push(field("deadline", FieldType::DateTime, None));
            // A child DocType's link field is not an edge (children are never graph nodes), so
            // `account_line.account` is the shape that is a field but not a readable one.
            let mut line = Definition::new("account_line");
            line.is_child = true;
            line.fields
                .push(field("account", FieldType::Link, Some("account")));
            account
                .fields
                .push(field("lines", FieldType::Table, Some("account_line")));
            vec![account, line]
        }
        "org" => {
            let mut employee = Definition::new("employee");
            employee
                .fields
                .push(field("manager", FieldType::Link, Some("employee")));
            employee
                .fields
                .push(field("delegate", FieldType::Link, Some("employee")));
            employee
                .fields
                .push(field("can_approve", FieldType::Bool, None));
            vec![employee]
        }
        "bom" => {
            let mut item = Definition::new("item");
            item.fields
                .push(field("component", FieldType::Link, Some("item")));
            let mut change = Definition::new("bom_change");
            change
                .fields
                .push(field("item", FieldType::Link, Some("item")));
            vec![item, change]
        }
        other => panic!("unknown corpus metadata `{other}`"),
    };
    Registry::new(definitions)
        .unwrap_or_else(|errors| panic!("metadata builds: {errors:?}"))
        .effective()
}

/// A deterministic record key: seed 0..15 makes a distinct UUIDv7.
pub fn rec_id(seed: u8) -> RecordId {
    let mut bytes = [0_u8; 16];
    bytes[6] = 0x70 | (seed & 0x0f);
    bytes[8] = 0x80 | (seed & 0x3f);
    bytes[15] = seed;
    RecordId::from_bytes(bytes).unwrap_or_else(|e| panic!("record id builds: {e}"))
}

/// A record key of `entity` at `seed`.
pub fn rec(entity: &str, seed: u8) -> NodeRef {
    NodeRef {
        entity: Name::new(entity).unwrap_or_else(|e| panic!("entity name builds: {e}")),
        id: rec_id(seed),
    }
}

/// The seed of a record key, for comparing against an oracle.
pub fn seed_of(node: &NodeRef) -> u8 {
    node.id.as_bytes()[15]
}

/// A decimal storage value.
pub fn decimal(text: &str) -> Value {
    Value::Decimal(text.parse::<Decimal>().unwrap_or_default())
}
