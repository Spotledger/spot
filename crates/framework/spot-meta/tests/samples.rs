//! The three sample DocTypes: parse, round trip, app API view and storage catalog.
#![allow(missing_docs)]

mod common;

use common::{effective, fixtures};
use spot_app_api::meta::FieldKind;
use spot_meta::{Definition, FieldType};
use spot_storage_contract::Name;
use spot_storage_contract::StorageBackend;
use spot_storage_contract::schema::{
    Collation, EntityKind, FieldType as Storage, LinkCheck, LinkTarget,
};

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn en() -> Collation {
    Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn load_dir_is_sorted_and_complete() {
    let names: Vec<_> = fixtures().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["invoice", "invoice_line", "party"]);
}

#[test]
fn definitions_round_trip_through_toml() {
    for d in fixtures() {
        let text = d.to_toml().unwrap_or_else(|e| panic!("{e}"));
        let back = Definition::parse("round-trip", &text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, d, "{}", d.name);
    }
}

#[test]
fn unknown_keys_name_the_file_and_the_key() {
    let bad = Definition::parse("thing.toml", "name = \"thing\"\nlabell = \"x\"\n");
    let err = bad.err().unwrap_or_else(|| panic!("must fail")).to_string();
    assert!(err.contains("thing.toml"), "{err}");
    assert!(err.contains("labell"), "{err}");

    let field = "name = \"t\"\n[[fields]]\nname = \"a\"\ntype = \"text\"\nmax_char = 3\n";
    let err = Definition::parse("t.toml", field)
        .err()
        .map(|e| e.to_string());
    assert!(err.is_some_and(|e| e.contains("max_char")));
}

#[test]
fn engine_fields_are_added_to_posted_doctypes_only() {
    let meta = effective();
    let invoice = meta.doctype("invoice").unwrap_or_else(|| panic!("invoice"));
    let tail: Vec<_> = invoice
        .fields
        .iter()
        .rev()
        .take(2)
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(tail, ["amended_from", "doc_state"]);
    let party = meta.doctype("party").unwrap_or_else(|| panic!("party"));
    assert!(party.field("doc_state").is_none());
    assert!(party.field("amended_from").is_none());
}

#[test]
fn app_view_of_the_samples() {
    let meta = effective();
    let inv = meta.doctype("invoice").unwrap_or_else(|| panic!("invoice"));
    assert_eq!(inv.name, n("invoice"));
    assert_eq!(inv.label, "Invoice");
    assert!(inv.is_posted && !inv.is_child);
    assert_eq!(inv.title_field, Some(n("number")));
    assert_eq!(inv.version, meta.version());
    let kinds: Vec<_> = inv
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.kind.clone()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                "number",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain
                }
            ),
            (
                "party",
                FieldKind::Link {
                    doctype: n("party")
                }
            ),
            ("posting_date", FieldKind::Date),
            (
                "status",
                FieldKind::Select {
                    values: ["Open", "Paid", "Overdue"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }
            ),
            (
                "lines",
                FieldKind::Table {
                    doctype: n("invoice_line")
                }
            ),
            (
                "total",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain
                }
            ),
            (
                "remarks",
                FieldKind::LongText {
                    format: LongTextFormat::Plain
                }
            ),
            (
                "doc_state",
                FieldKind::Text {
                    max_chars: Some(16),
                    format: TextFormat::Plain
                }
            ),
            (
                "amended_from",
                FieldKind::Link {
                    doctype: n("invoice")
                }
            ),
        ]
    );
    assert!(inv.field("party").is_some_and(|f| f.required));
    assert!(inv.field("status").is_some_and(|f| f.allow_on_posted));
    assert!(inv.field("total").is_some_and(|f| !f.allow_on_posted));
    // ADR-068 §6: the view carries the label, read_only and the parsed default.
    assert!(inv.field("total").is_some_and(|f| f.read_only));
    assert!(inv.field("posting_date").is_some_and(|f| !f.read_only));
    assert_eq!(
        inv.field("number").map(|f| f.label.as_str()),
        Some("Number")
    );
    assert!(inv.field("remarks").is_some_and(|f| f.label == "remarks"));
    use spot_app_api::meta::{DecimalFormat, LongTextFormat, TextFormat};
    use spot_storage_contract::Value;
    let status_default = inv
        .field("status")
        .and_then(|f| f.default.as_ref())
        .unwrap_or_else(|| panic!("status has a default"));
    assert_eq!(status_default, &Value::Text("Open".into()));
    assert!(inv.field("party").is_some_and(|f| f.default.is_none()));
    assert_eq!(inv.tables().get("lines"), Some(&&n("invoice_line")));

    let line = meta
        .doctype("invoice_line")
        .unwrap_or_else(|| panic!("line"));
    assert!(line.is_child && !line.is_posted);
    assert_eq!(line.title_field, None);
    let qty = line.field("qty").unwrap_or_else(|| panic!("qty"));
    assert!(matches!(
        qty.default,
        Some(Value::Decimal(d)) if d == spot_app_api::prelude::Decimal::from(1)
    ));

    // ADR-068 §6: the dynamic link is in the view with its type field.
    let party = meta.doctype("party").unwrap_or_else(|| panic!("party"));
    let names: Vec<_> = party.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["code", "active", "about_type", "about", "since"]);
    assert_eq!(
        party.field("about").map(|f| f.kind.clone()),
        Some(FieldKind::DynamicLink {
            type_field: n("about_type")
        })
    );
    let active = party.field("active").unwrap_or_else(|| panic!("active"));
    assert_eq!(active.default, Some(Value::Bool(true)));
    let since = party.field("since").unwrap_or_else(|| panic!("since"));
    assert!(since.default.is_some(), "the datetime default parses");
}

#[test]
fn catalog_of_the_samples() {
    let meta = effective();
    let cat = meta.catalog(en()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cat.version, meta.version());

    let inv = cat.entity(&n("invoice")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(inv.kind, EntityKind::Document);
    assert!(inv.fields.iter().all(|f| f.nullable));
    let ty = |name: &str| {
        inv.field(&n(name))
            .unwrap_or_else(|e| panic!("{e}"))
            .ty
            .clone()
    };
    assert_eq!(ty("status"), Storage::Text { max_chars: None });
    assert_eq!(ty("remarks"), Storage::Text { max_chars: None });
    assert_eq!(
        ty("total"),
        Storage::Decimal {
            precision: 18,
            scale: 2
        }
    );
    assert_eq!(
        ty("party"),
        Storage::Link {
            target: LinkTarget::Entity(n("party")),
            check: LinkCheck::Database
        }
    );
    assert_eq!(
        ty("amended_from"),
        Storage::Link {
            target: LinkTarget::Entity(n("invoice")),
            check: LinkCheck::Database
        }
    );
    assert!(inv.field(&n("lines")).is_err(), "a table is not a column");
    assert_eq!(inv.children.len(), 1);
    assert_eq!(inv.children[0].entity, n("invoice_line"));
    let ix: Vec<_> = inv
        .indexes
        .iter()
        .map(|i| (i.name.as_str(), i.unique))
        .collect();
    assert_eq!(
        ix,
        vec![
            ("uq_invoice_number", true),
            ("ix_invoice_posting_date", false),
            ("ix_invoice_party_posting_date", false),
        ]
    );

    let line = cat
        .entity(&n("invoice_line"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        line.kind,
        EntityKind::Child {
            parent: n("invoice")
        }
    );
    assert_eq!(
        line.field(&n("amount")).map(|f| f.ty.clone()).ok(),
        Some(Storage::Decimal {
            precision: 20,
            scale: 2
        })
    );

    let party = cat.entity(&n("party")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        party.field(&n("about")).map(|f| f.ty.clone()).ok(),
        Some(Storage::Link {
            target: LinkTarget::Dynamic {
                entity_field: n("about_type")
            },
            check: LinkCheck::EngineOnly {
                reason: "dynamic links are engine-checked".into()
            },
        })
    );
    assert!(
        party
            .indexes
            .iter()
            .any(|i| i.name == n("uq_party_code") && i.unique)
    );
}

#[test]
fn compiling_is_deterministic() {
    let (a, b) = (effective(), effective());
    assert_eq!(a.catalog(en()).ok(), b.catalog(en()).ok());
    let docs =
        |m: &spot_meta::EffectiveMeta| m.doctypes().map(|d| (**d).clone()).collect::<Vec<_>>();
    assert_eq!(docs(&a), docs(&b));
}

#[tokio::test]
async fn storage_accepts_the_catalog() {
    let cat = effective().catalog(en()).unwrap_or_else(|e| panic!("{e}"));
    let backend = spot_storage_fake::FakeBackend::new();
    backend
        .prepare(&cat)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn field_types_are_the_documented_ones() {
    assert!(FieldType::Select.is_text_like() && !FieldType::Date.is_text_like());
}
