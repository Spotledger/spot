//! Shared fixtures: a sales-invoice-shaped catalogue, its policy and its protection model.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bi_views::{BiPolicy, Protection};
use spot_permissions::GroupName;
use spot_storage_contract::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, FieldDef, FieldType, LinkCheck,
    LinkTarget, Name,
};

pub fn n(text: &str) -> Name {
    Name::new(text).unwrap()
}

pub fn field(name: &str, ty: FieldType, nullable: bool) -> FieldDef {
    FieldDef {
        name: n(name),
        ty,
        nullable,
    }
}

pub fn text() -> FieldType {
    FieldType::Text { max_chars: None }
}

pub fn dec(precision: u8, scale: u8) -> FieldType {
    FieldType::Decimal { precision, scale }
}

pub fn link(target: &str) -> FieldType {
    FieldType::Link {
        target: LinkTarget::Entity(n(target)),
        check: LinkCheck::Database,
    }
}

/// `party` (a document with a protected tax id), `sales_invoice` (a document with a posted
/// marker, a link, a decimal, a date, a protected margin, a secret token, a JSON field and a
/// child table) and `sales_invoice_line` (its child table).
pub fn fixture_entities() -> Vec<EntityDef> {
    let party = EntityDef {
        name: n("party"),
        kind: EntityKind::Document,
        fields: vec![field("name", text(), false), field("tax_id", text(), true)],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let invoice = EntityDef {
        name: n("sales_invoice"),
        kind: EntityKind::Document,
        fields: vec![
            field("docstatus", FieldType::Int, false),
            field("customer", link("party"), true),
            field("posting_date", FieldType::Date, true),
            field("total", dec(18, 2), true),
            field("margin", dec(18, 2), true),
            field("api_token", text(), true),
            field("remarks", text(), true),
            field("flags", FieldType::Json, true),
            field("seen_at", FieldType::DateTime, true),
            field("is_hold", FieldType::Bool, false),
            field("at_time", FieldType::Time, true),
        ],
        children: vec![ChildTableDef {
            field: n("lines"),
            entity: n("sales_invoice_line"),
        }],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let line = EntityDef {
        name: n("sales_invoice_line"),
        kind: EntityKind::Child {
            parent: n("sales_invoice"),
        },
        fields: vec![
            field("item", text(), true),
            field("qty", dec(18, 3), true),
            field("rate", dec(18, 2), true),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    vec![party, invoice, line]
}

pub fn catalog_of(version: u64, entities: Vec<EntityDef>) -> Catalog {
    Catalog::new(version, Collation::new("en").unwrap(), entities).unwrap()
}

pub fn fixture_catalog(version: u64) -> Catalog {
    catalog_of(version, fixture_entities())
}

pub fn fixture_protection() -> Protection {
    Protection::new()
        .group_field("party", "tax_id", &GroupName("pii".to_owned()))
        .unwrap()
        .group_field("sales_invoice", "margin", &GroupName("cost".to_owned()))
        .unwrap()
        .secret_field("sales_invoice", "api_token")
        .unwrap()
}

pub const FIXTURE_POLICY: &str = r#"
expose = ["party", "sales_invoice"]

[entity.party]
posted_only = false
title_field = "name"

[entity.sales_invoice]
display = "Sales invoices"
children = true
exclude = ["remarks"]
labels = { total = "Total amount", seen_at = "it's a \\ test" }

[entity.sales_invoice_line]
labels = { qty = "Quantity" }
"#;

pub fn policy(text: &str) -> BiPolicy {
    BiPolicy::from_toml(text).unwrap_or_else(|e| panic!("{e}"))
}

pub fn fixture_policy() -> BiPolicy {
    policy(FIXTURE_POLICY)
}
