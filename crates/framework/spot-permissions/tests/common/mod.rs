//! Shared fixtures: a DocType set with protection groups, the grant tables (loaded through the
//! public loader, so the loader is exercised on every run), and the matrix readers' helpers.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_app_api::prelude::{RecordId, Value};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::model::{
    FieldFacts, GroupName, MaskFormat, Model, ModelTexts, Principal, Tag,
};
use spot_permissions::{PermissionEngine, Snapshot, load};

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn required(mut f: FieldSpec) -> FieldSpec {
    f.required = true;
    f
}

fn decimal(name: &str, scale: u8) -> FieldSpec {
    let mut f = field(name, FieldType::Decimal);
    f.scale = Some(scale);
    f
}

/// The DocType set:
/// - `customer`: a plain master with one text field.
/// - `sales_invoice`: posted, with `customer` (link), `number`, `status` (select), `total`,
///   `internal_notes`, `bank_account` and a child table `items`.
/// - `sales_invoice_item`: its child, with an `item` link to `item_master`.
/// - `item_master`: a second master with a `cost_price`.
/// - `project`: a plain (non-posted) DocType with a `customer` link.
pub fn definitions() -> Vec<Definition> {
    let mut customer_doc = Definition::new("customer");
    customer_doc.fields = vec![required(field("customer_name", FieldType::Text))];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let number = required(field("number", FieldType::Text));
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
        decimal("total", 2),
        field("internal_notes", FieldType::LongText),
        field("bank_account", FieldType::Text),
        items,
    ];

    let mut item_doc = Definition::new("sales_invoice_item");
    item_doc.is_child = true;
    let mut item_link = required(field("item", FieldType::Link));
    item_link.options = Some("item_master".into());
    item_doc.fields = vec![item_link, required(decimal("qty", 3))];

    let mut item_master = Definition::new("item_master");
    item_master.fields = vec![
        required(field("item_name", FieldType::Text)),
        decimal("cost_price", 2),
    ];

    let mut project = Definition::new("project");
    let mut pc = required(field("customer", FieldType::Link));
    pc.options = Some("customer".into());
    project.fields = vec![field("project_name", FieldType::Text), pc];

    vec![customer_doc, invoice, item_doc, item_master, project]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// The DocType grants, by the role names the matrices use.
pub const GRANTS: &str = r#"
[[entries]]
role = "Reader"
doctype = "project"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "sales_invoice"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "item_master"
rights = ["read"]
[[entries]]
role = "Viewer"
doctype = "project"
rights = ["read"]
[[entries]]
role = "Manager"
doctype = "project"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct", "report"]
[[entries]]
role = "Writer"
doctype = "project"
rights = ["read", "write"]
[[entries]]
role = "Writer"
doctype = "sales_invoice"
rights = ["read", "write"]
[[entries]]
role = "Writer"
doctype = "item_master"
rights = ["read", "write"]
[[entries]]
role = "Editor"
doctype = "sales_invoice"
rights = ["read", "write", "update_posted"]
[[entries]]
role = "Submitter"
doctype = "sales_invoice"
rights = ["read", "submit"]
[[entries]]
role = "Canceler"
doctype = "sales_invoice"
rights = ["read", "cancel"]
[[entries]]
role = "Corrector"
doctype = "sales_invoice"
rights = ["read", "create"]
[[entries]]
role = "Creator"
doctype = "sales_invoice"
rights = ["read", "create"]
[[entries]]
role = "Creator"
doctype = "item_master"
rights = ["read", "create"]
[[entries]]
role = "Reporter"
doctype = "sales_invoice"
rights = ["read", "report"]
[[entries]]
role = "Reporter"
doctype = "item_master"
rights = ["read", "report"]
[[entries]]
role = "OwnerRole"
doctype = "project"
rights = ["read", "write"]
if_owner = true
"#;

pub const GROUPS: &str = r#"
[[entries]]
name = "cost"
trace = true
[[entries]]
name = "notes"
[[entries]]
name = "financial"
[[entries]]
name = "credit"
[[entries]]
name = "bank"
trace = true
"#;

/// The field grants, by the role names the field matrix uses.
pub const FIELD_GRANTS: &str = r#"
# financial and credit: the two groups of `total`
[[entries]]
role = "FinRead"
doctype = "sales_invoice"
target = { group = "financial" }
access = ["read"]
[[entries]]
role = "FinRead"
doctype = "sales_invoice"
target = { group = "credit" }
access = ["read"]
[[entries]]
role = "FinFinancialOnly"
doctype = "sales_invoice"
target = { group = "financial" }
access = ["read"]
[[entries]]
role = "CreditRead"
doctype = "sales_invoice"
target = { group = "credit" }
access = ["read"]
[[entries]]
role = "FinMasked"
doctype = "sales_invoice"
target = { group = "financial" }
access = ["read_masked"]
[[entries]]
role = "FinMasked"
doctype = "sales_invoice"
target = { group = "credit" }
access = ["read_masked"]
[[entries]]
role = "FinMix"
doctype = "sales_invoice"
target = { group = "financial" }
access = ["read"]
[[entries]]
role = "FinMix"
doctype = "sales_invoice"
target = { group = "credit" }
access = ["read_masked"]
# cost
[[entries]]
role = "CostRead"
doctype = "item_master"
target = { group = "cost" }
access = ["read"]
[[entries]]
role = "CostUpdate"
doctype = "item_master"
target = { group = "cost" }
access = ["update"]
[[entries]]
role = "CostCreate"
doctype = "item_master"
target = { group = "cost" }
access = ["create"]
[[entries]]
role = "CostReport"
doctype = "item_master"
target = { group = "cost" }
access = ["report"]
# notes
[[entries]]
role = "NotesRead"
doctype = "sales_invoice"
target = { group = "notes" }
access = ["read"]
[[entries]]
role = "NotesUpdate"
doctype = "sales_invoice"
target = { group = "notes" }
access = ["update"]
[[entries]]
role = "NotesDeny"
doctype = "sales_invoice"
target = { group = "notes" }
deny = true
[[entries]]
role = "NotesDenyRead"
doctype = "sales_invoice"
target = { group = "notes" }
access = ["read"]
deny = true
[[entries]]
role = "OwnNotes"
doctype = "sales_invoice"
target = { group = "notes" }
access = ["read"]
condition = "own_record"
[[entries]]
role = "NotesUpdateDraft"
doctype = "sales_invoice"
target = { group = "notes" }
access = ["update"]
condition = { status_in = ["draft"] }
# bank
[[entries]]
role = "BankMasked"
doctype = "sales_invoice"
target = { group = "bank" }
access = ["read_masked"]
[[entries]]
role = "BankReveal"
doctype = "sales_invoice"
target = { group = "bank" }
access = ["reveal"]
[[entries]]
role = "BankReadReveal"
doctype = "sales_invoice"
target = { group = "bank" }
access = ["read", "reveal"]
[[entries]]
role = "BankBulk"
doctype = "sales_invoice"
target = { group = "bank" }
access = ["reveal_bulk"]
# a field grant on `number` (no group): the field is protected by the grant alone
[[entries]]
role = "NumberRead"
doctype = "sales_invoice"
target = { field = "number" }
access = ["read"]
[[entries]]
role = "NumberUpdate"
doctype = "sales_invoice"
target = { field = "number" }
access = ["update"]
"#;

/// The shares: `sam` may read `doc_a` and read and write `doc_b`.
pub fn shares_text() -> String {
    format!(
        r#"
[[entries]]
doc = "{}"
user = "sam"
rights = ["read"]
[[entries]]
doc = "{}"
user = "sam"
rights = ["read", "write"]
"#,
        doc_id("doc_a"),
        doc_id("doc_b")
    )
}

pub fn model() -> Model {
    load(&ModelTexts {
        grants: GRANTS.to_owned(),
        field_grants: FIELD_GRANTS.to_owned(),
        groups: GROUPS.to_owned(),
        shares: shares_text(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

fn gn(name: &str) -> GroupName {
    GroupName(name.to_owned())
}

/// The protection facts of the fixture fields.
pub fn field_facts() -> Vec<(&'static str, Vec<FieldFacts>)> {
    vec![
        (
            "sales_invoice",
            vec![
                FieldFacts {
                    path: "total".into(),
                    groups: vec![gn("financial"), gn("credit")],
                    tags: vec![Tag("financial".into())],
                    mask: None,
                },
                FieldFacts {
                    path: "internal_notes".into(),
                    groups: vec![gn("notes")],
                    tags: vec![],
                    mask: None,
                },
                FieldFacts {
                    path: "bank_account".into(),
                    groups: vec![gn("bank")],
                    tags: vec![Tag("secret".into())],
                    mask: Some(MaskFormat::LastChars(4)),
                },
            ],
        ),
        (
            "item_master",
            vec![FieldFacts {
                path: "cost_price".into(),
                groups: vec![gn("cost")],
                tags: vec![Tag("financial".into())],
                mask: Some(MaskFormat::Placeholder),
            }],
        ),
    ]
}

pub fn snapshot() -> Snapshot {
    let mut snap =
        Snapshot::build(&meta(), &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    for (doctype, facts) in field_facts() {
        snap = snap
            .with_field_facts(doctype, facts)
            .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    }
    snap
}

pub fn engine() -> PermissionEngine {
    PermissionEngine::new(Arc::new(snapshot()))
}

/// A principal with one unscoped assignment holding `roles`.
pub fn user(name: &str, roles: &[&str]) -> Principal {
    Principal::with_roles(name, roles)
}

/// Deterministic UUIDv7 distinct per (site, seq), the same pattern spot-activity's tests use.
pub fn uuid7(site: u32, seq: u32) -> RecordId {
    let unix_ms: i64 = 1_790_000_000 * 1000 + i64::from(site) * 0x1000 + i64::from(seq);
    let mut b = [0u8; 16];
    b[0] = (unix_ms >> 40) as u8;
    b[1] = (unix_ms >> 32) as u8;
    b[2] = (unix_ms >> 24) as u8;
    b[3] = (unix_ms >> 16) as u8;
    b[4] = (unix_ms >> 8) as u8;
    b[5] = unix_ms as u8;
    b[6] = 0x70 | ((seq >> 8) as u8 & 0x0f);
    b[7] = seq as u8;
    b[8] = 0x80 | (site as u8 & 0x3f);
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

/// The RecordId the matrix's doc alias names (`doc_a`..=doc_c; stable test addresses).
pub fn doc_id(alias: &str) -> spot_app_api::prelude::RecordId {
    let seq = match alias {
        "doc_a" => (300, 1),
        "doc_b" => (300, 2),
        "doc_c" => (300, 3),
        other => panic!("unknown doc alias `{other}`"),
    };
    uuid7(seq.0, seq.1)
}

/// A matrix `id` cell: a `doc_x` alias as its canonical id text, anything else verbatim.
pub fn resolve_doc_id(cell: &str) -> String {
    match cell {
        "doc_a" | "doc_b" | "doc_c" => doc_id(cell).to_string(),
        other => other.to_owned(),
    }
}

/// A matrix `values` cell: the `customer_N` aliases resolve to the common fixtures' record
/// ids, anything else (a select value) stays text.
pub fn resolve_value(cell: &str) -> Value {
    let seq = match cell {
        "customer_1" => (400, 1),
        "customer_2" => (400, 2),
        "customer_3" => (400, 3),
        _ => return Value::Text(cell.to_owned()),
    };
    Value::Id(uuid7(seq.0, seq.1))
}
