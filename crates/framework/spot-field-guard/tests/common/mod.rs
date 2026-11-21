//! Shared fixtures: a DocType set with protection groups built through the real metadata
//! (so the `MetaProtection` adapter and the inheritance of derived fields are exercised on
//! every run), a permission model with six principals, and a document planted with the
//! leak corpus.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Decimal, Name, RecordId, Value};
use spot_core::ManualClock;
use spot_field_guard::leakcheck::{Corpus, Output, Secret};
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, Loaded, MetaProtection,
    ProtectionSource, ValueSource,
};
use spot_meta::{
    BuildOptions, Definition, Derivation, EffectiveMeta, FieldRef, FieldSpec, FieldType, MaskSpec,
    ProtectionGroupSpec, Registry,
};
use spot_permissions::model::Principal;
use spot_permissions::{DocFacts, Model, ModelTexts, PermissionEngine, Snapshot, load};

pub const DOCTYPE: &str = "sales_invoice";
pub const CHILD: &str = "sales_invoice_item";

fn grouped(name: &str, ty: FieldType, group: &str, mask: Option<MaskSpec>) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.protection_group = Some(group.to_owned());
    f.masking = mask;
    f
}

pub fn definitions() -> Vec<Definition> {
    let mut invoice = Definition::new(DOCTYPE);
    let mut items = FieldSpec::new("items", FieldType::Table);
    items.options = Some(CHILD.into());
    let mut bonus = grouped(
        "bonus",
        FieldType::Decimal,
        "finance",
        Some(MaskSpec::placeholder()),
    );
    bonus.scale = Some(2);
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        FieldSpec::new("notes", FieldType::LongText),
        grouped("card_number", FieldType::Text, "pci", Some(last(4))),
        grouped(
            "holder",
            FieldType::Text,
            "pci",
            Some(MaskSpec::placeholder()),
        ),
        grouped(
            "profile_json",
            FieldType::LongText,
            "pci",
            Some(MaskSpec::placeholder()),
        ),
        grouped(
            "salary",
            FieldType::Text,
            "hr",
            Some(MaskSpec::placeholder()),
        ),
        bonus,
        grouped(
            "annual_cost",
            FieldType::Text,
            "hr",
            Some(MaskSpec::placeholder()),
        ),
        items,
    ];
    invoice.protection_groups = vec![
        ProtectionGroupSpec::new("pci", "Payment cards"),
        ProtectionGroupSpec {
            trace: true,
            ..ProtectionGroupSpec::new("hr", "Compensation")
        },
        ProtectionGroupSpec::new("finance", "Finance"),
    ];

    let mut item = Definition::new(CHILD);
    item.is_child = true;
    let mut qty = FieldSpec::new("qty", FieldType::Decimal);
    qty.scale = Some(3);
    item.fields = vec![
        qty,
        grouped("iban", FieldType::Text, "bank", Some(last(4))),
        grouped(
            "memo",
            FieldType::Text,
            "bank",
            Some(MaskSpec::placeholder()),
        ),
    ];
    item.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    vec![invoice, item]
}

fn last(n: u8) -> MaskSpec {
    MaskSpec {
        placeholder: false,
        last_chars: Some(n),
    }
}

/// `annual_cost` is computed from `salary` (hr) and `bonus` (finance): it is in `hr` itself and
/// must inherit `finance`, so it needs a grant in both.
pub fn derivations() -> Vec<Derivation> {
    vec![Derivation::new(
        FieldRef::new(DOCTYPE, "annual_cost"),
        vec![
            FieldRef::new(DOCTYPE, "salary"),
            FieldRef::new(DOCTYPE, "bonus"),
        ],
    )]
}

pub fn meta() -> Arc<EffectiveMeta> {
    let options = BuildOptions::new().with_derivations(&derivations());
    Registry::with_options(definitions(), options)
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

const GROUPS: &str = r#"
[[entries]]
name = "pci"
[[entries]]
name = "hr"
trace = true
[[entries]]
name = "finance"
[[entries]]
name = "bank"
"#;

fn grants() -> String {
    let mut out = String::new();
    for (role, rights) in [
        ("Clerk", r#"["read"]"#),
        ("MaskedReader", r#"["read", "report"]"#),
        ("Analyst", r#"["read", "report"]"#),
        ("ReadNoReport", r#"["read"]"#),
    ] {
        for doctype in [DOCTYPE, CHILD] {
            out.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = {rights}\n"
            ));
        }
    }
    out
}

fn field_grants() -> String {
    let mut out = String::new();
    let mut add = |role: &str, doctype: &str, group: &str, access: &str| {
        out.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\n"
        ));
    };
    for (doctype, group) in [
        (DOCTYPE, "pci"),
        (DOCTYPE, "hr"),
        (DOCTYPE, "finance"),
        (CHILD, "bank"),
    ] {
        add(
            "MaskedReader",
            doctype,
            group,
            r#"["read_masked", "report"]"#,
        );
        add("Analyst", doctype, group, r#"["read", "report"]"#);
    }
    // ReadNoReport holds `read` on two groups only, and no `report` anywhere.
    add("ReadNoReport", DOCTYPE, "pci", r#"["read"]"#);
    add("ReadNoReport", DOCTYPE, "hr", r#"["read"]"#);
    out
}

pub fn model() -> Model {
    load(&ModelTexts {
        grants: grants(),
        field_grants: field_grants(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

pub fn snapshot(meta: &EffectiveMeta, protection: &dyn ProtectionSource) -> Snapshot {
    let mut snap = Snapshot::build(meta, &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    for doctype in [DOCTYPE, CHILD] {
        let prot = protection.protection(doctype).unwrap();
        snap = snap
            .with_field_facts(doctype, prot.fields.clone())
            .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    }
    snap
}

pub struct Fixture {
    pub guard: FieldGuard,
    pub meta: Arc<EffectiveMeta>,
    pub protection: Arc<MetaProtection>,
    pub trace: Arc<BoundedQueue<FieldAccessRecord>>,
    pub audit: Arc<BoundedQueue<AuditRecord>>,
    pub clock: Arc<ManualClock>,
}

pub fn fixture() -> Fixture {
    fixture_with(1024)
}

pub fn fixture_with(queue: usize) -> Fixture {
    let meta = meta();
    let protection = Arc::new(MetaProtection::new(&meta));
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot(
        &meta,
        &*protection,
    ))));
    let trace = Arc::new(BoundedQueue::new(queue));
    let audit = Arc::new(BoundedQueue::new(queue));
    let clock = Arc::new(ManualClock::new(
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    ));
    let guard = FieldGuard::new(
        engine,
        protection.clone(),
        trace.clone(),
        audit.clone(),
        clock.clone(),
    );
    Fixture {
        guard,
        meta,
        protection,
        trace,
        audit,
        clock,
    }
}

// ----------------------------------------------------------------- principals

pub const PRINCIPALS: [&str; 6] = [
    "nobody",
    "clerk",
    "masked_reader",
    "analyst",
    "read_no_report",
    "admin",
];

pub fn principal(name: &str) -> Principal {
    match name {
        "nobody" => Principal::with_roles("nobody", &[]),
        "clerk" => Principal::with_roles("clerk", &["Clerk"]),
        "masked_reader" => Principal::with_roles("masked_reader", &["MaskedReader"]),
        "analyst" => Principal::with_roles("analyst", &["Analyst"]),
        "read_no_report" => Principal::with_roles("read_no_report", &["ReadNoReport"]),
        "admin" => Principal::with_roles("admin", &[spot_permissions::TENANT_ADMINISTRATOR]),
        other => panic!("unknown principal {other}"),
    }
}

/// The groups a principal sees **unmasked** (read right), and the groups it sees unmasked
/// through `report` (a channel that needs `report`). Plain fields: `read` / `report` on the
/// DocType.
pub struct Expect {
    pub read_groups: &'static [&'static str],
    pub report_groups: &'static [&'static str],
    pub plain_read: bool,
    pub plain_report: bool,
}

pub fn expect(name: &str) -> Expect {
    const ALL: &[&str] = &["pci", "hr", "finance", "bank"];
    match name {
        "nobody" => Expect {
            read_groups: &[],
            report_groups: &[],
            plain_read: false,
            plain_report: false,
        },
        "clerk" => Expect {
            read_groups: &[],
            report_groups: &[],
            plain_read: true,
            plain_report: false,
        },
        "masked_reader" => Expect {
            read_groups: &[],
            report_groups: &[],
            plain_read: true,
            plain_report: true,
        },
        "analyst" | "admin" => Expect {
            read_groups: ALL,
            report_groups: ALL,
            plain_read: true,
            plain_report: true,
        },
        "read_no_report" => Expect {
            read_groups: &["pci", "hr"],
            report_groups: &[],
            plain_read: true,
            plain_report: false,
        },
        other => panic!("unknown principal {other}"),
    }
}

// ----------------------------------------------------------------- the corpus and document

/// Corpus label to the groups of the field that holds it.
pub const SECRET_GROUPS: [(&str, &[&str]); 7] = [
    ("card", &["pci"]),
    ("lookalike", &["pci"]),
    ("in_json", &["pci"]),
    ("salary", &["hr"]),
    ("annual", &["finance", "hr"]),
    ("iban", &["bank"]),
    ("split_rows", &["bank"]),
];

pub fn corpus() -> Corpus {
    Corpus::standard().with(Secret::whole("annual", "1014009.00"))
}

pub const PLAIN_NUMBER: &str = "INV-0001";
pub const PLAIN_NOTE: &str = "ship early";

pub fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub fn invoice(corpus: &Corpus) -> Document {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let split = corpus.get("split_rows").unwrap().planted.clone();
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name("number"), Value::Text(PLAIN_NUMBER.into()));
    doc.set(name("notes"), Value::Text(PLAIN_NOTE.into()));
    doc.set(name("card_number"), Value::Text(value("card")));
    doc.set(name("holder"), Value::Text(value("lookalike")));
    doc.set(name("profile_json"), Value::Text(value("in_json")));
    doc.set(name("salary"), Value::Text(value("salary")));
    doc.set(name("bonus"), Value::Decimal(Decimal::new(150_050, 2)));
    doc.set(name("annual_cost"), Value::Text(value("annual")));
    for (i, memo) in split.iter().enumerate() {
        let mut fields = BTreeMap::new();
        fields.insert(
            name("qty"),
            Value::Decimal(Decimal::new(2000 + i as i64, 3)),
        );
        fields.insert(name("iban"), Value::Text(value("iban")));
        fields.insert(name("memo"), Value::Text(memo.clone()));
        doc.rows_mut(name("items")).push(ChildRow {
            id: RecordId::new_v7(),
            fields,
        });
    }
    doc
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(t) => t.clone(),
        Value::Decimal(d) => d.to_string(),
        Value::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

pub fn to_json(doc: &Document) -> String {
    let mut fields = serde_json::Map::new();
    for (k, v) in doc.fields() {
        fields.insert(
            k.as_str().to_owned(),
            serde_json::Value::String(value_text(v)),
        );
    }
    let mut tables = serde_json::Map::new();
    for (t, rows) in doc.tables() {
        let rows: Vec<_> = rows
            .iter()
            .map(|r| {
                let mut o = serde_json::Map::new();
                for (k, v) in &r.fields {
                    o.insert(
                        k.as_str().to_owned(),
                        serde_json::Value::String(value_text(v)),
                    );
                }
                serde_json::Value::Object(o)
            })
            .collect();
        tables.insert(t.as_str().to_owned(), serde_json::Value::Array(rows));
    }
    serde_json::json!({ "fields": fields, "tables": tables }).to_string()
}

pub fn to_csv(doc: &Document) -> String {
    let mut out = String::new();
    for (k, v) in doc.fields() {
        out.push_str(&format!("{},{}\n", k.as_str(), value_text(v)));
    }
    for (t, rows) in doc.tables() {
        for r in rows {
            for (k, v) in &r.fields {
                out.push_str(&format!(
                    "{}.{},{}\n",
                    t.as_str(),
                    k.as_str(),
                    value_text(v)
                ));
            }
        }
    }
    out
}

pub fn outputs(doc: &Document) -> Vec<Output> {
    vec![Output::Json(to_json(doc)), Output::Csv(to_csv(doc))]
}

pub fn doc_facts() -> DocFacts {
    DocFacts {
        doctype: DOCTYPE.into(),
        ..DocFacts::default()
    }
}

// ----------------------------------------------------------------- a value source

#[derive(Default)]
pub struct MapValues {
    rows: Mutex<BTreeMap<(String, String, String), Loaded>>,
}

impl MapValues {
    pub fn put(&self, doctype: &str, id: &str, field: &str, value: Value) {
        self.rows.lock().unwrap().insert(
            (doctype.to_owned(), id.to_owned(), field.to_owned()),
            Loaded {
                value,
                doc_facts: DocFacts::default(),
            },
        );
    }
}

impl ValueSource for MapValues {
    fn load(&self, doctype: &str, id: &str, field: &str) -> Option<Loaded> {
        self.rows
            .lock()
            .unwrap()
            .get(&(doctype.to_owned(), id.to_owned(), field.to_owned()))
            .cloned()
    }
}

pub fn groups_of(set: &[&str]) -> BTreeSet<String> {
    set.iter().map(|s| (*s).to_owned()).collect()
}
