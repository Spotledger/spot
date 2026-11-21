//! Shared fixtures: the metadata the engine is built over, the fixture registry, and helpers.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::prelude::{RecordId, Value};
use spot_iam_roles::{Registry, SnapshotInput};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry as MetaRegistry};
use spot_permissions::Snapshot;
use spot_permissions::engine::{Action, DocFacts, PermissionEngine};
use spot_permissions::model::UserPermission;
use time::OffsetDateTime;

pub const REGISTRY: &str = include_str!("../fixtures/registry.toml");

/// 2026-10-08 00:00:00 UTC plus `seconds`.
pub fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_791_417_600 + seconds).unwrap()
}

pub fn registry() -> Registry {
    Registry::from_toml(REGISTRY).unwrap_or_else(|e| panic!("{e}"))
}

/// `customer` and `sales_invoice` (posted, with a `customer` link and a `total`).
pub fn meta() -> Arc<EffectiveMeta> {
    let mut customer = Definition::new("customer");
    let mut name = FieldSpec::new("customer_name", FieldType::Text);
    name.required = true;
    customer.fields = vec![name];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut number = FieldSpec::new("number", FieldType::Text);
    number.required = true;
    let mut link = FieldSpec::new("customer", FieldType::Link);
    link.options = Some("customer".into());
    link.required = true;
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    invoice.fields = vec![number, link, total];

    MetaRegistry::new(vec![customer, invoice])
        .unwrap_or_else(|e| panic!("{e:?}"))
        .effective()
}

/// The engine over the registry's flattened rights.
pub fn engine(input: &SnapshotInput) -> PermissionEngine {
    let snapshot = Snapshot::build(&meta(), &input.model(vec![], vec![]))
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    PermissionEngine::new(Arc::new(snapshot))
}

/// Three customers with fixed ids, so scopes and documents built apart agree.
pub fn customers() -> BTreeMap<&'static str, RecordId> {
    [("c1", 1_u8), ("c2", 2), ("c3", 3)]
        .into_iter()
        .map(|(name, n)| {
            let mut bytes = [0x11_u8; 16];
            bytes[0] = n;
            bytes[6] = 0x70; // version 7
            bytes[8] = 0x80; // variant
            (name, RecordId::from_bytes(bytes).unwrap())
        })
        .collect()
}

/// A restriction to these customers, on every link to `customer`.
pub fn only_customers(user: &str, ids: &[RecordId]) -> UserPermission {
    UserPermission {
        user: user.to_owned(),
        doctype: None,
        field_or_link_doctype: "customer".to_owned(),
        allowed: ids.iter().map(ToString::to_string).collect(),
        applies_to: Default::default(),
        include_descendants: false,
    }
}

/// An invoice of customer `customer`.
pub fn invoice(customer: RecordId, state: &str) -> DocFacts {
    let mut values = BTreeMap::new();
    values.insert("customer".to_owned(), Value::Id(customer));
    DocFacts {
        doctype: "sales_invoice".into(),
        id: None,
        created_by: None,
        state: Some(state.into()),
        values,
    }
}

pub fn action(text: &str) -> Action {
    Action::parse(text).unwrap_or_else(|| panic!("unknown action `{text}`"))
}
