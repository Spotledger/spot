//! Shared fixtures for the `spot-plan` integration tests: one small, valid definition set the
//! registry accepts, covering every field kind the app API has.
#![allow(dead_code, missing_docs)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::prelude::{Decimal, Name, Value};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, MetaError, Registry};

/// The full fixture set: `file` (the platform file records), the posted `invoice`, the
/// `party` with a dynamic link and defaults, the child `invoice_line` and `pick`.
pub fn definitions() -> Vec<Definition> {
    vec![file(), invoice(), party(), invoice_line(), pick()]
}

fn file() -> Definition {
    let mut d = Definition::new("file");
    d.fields = vec![text("name")];
    d
}

fn invoice() -> Definition {
    let mut d = Definition::new("invoice");
    d.label = Some("Invoice".into());
    d.is_posted = true;
    d.title_field = Some("number".into());
    let mut number = text("number");
    number.max_chars = Some(140);
    number.unique = true;
    let mut status = of(FieldType::Select, "status", Some("Open\nPaid"));
    status.default = Some("Open".into());
    status.allow_on_posted = true;
    let mut tag = text("tag");
    tag.max_chars = Some(5);
    let mut total = of(FieldType::Decimal, "total", None);
    total.scale = Some(2);
    total.read_only = true;
    total.mandatory_depends_on = Some("doc.status == 'Paid'".into());
    let mut posting_date = set_required(of(FieldType::Date, "posting_date", None));
    posting_date.depends_on = Some("doc.status != 'Cancelled'".into());
    let mut remarks = of(FieldType::LongText, "remarks", None);
    remarks.read_only_depends_on = Some("doc.status == 'Paid'".into());
    // An expression default: the due date starts at the posting date.
    let mut due_date = of(FieldType::Date, "due_date", None);
    due_date.default = Some("=doc.posting_date".into());
    // A fetch_from: the party's code is copied from the linked party.
    let mut party_name = text("party_name");
    party_name.fetch_from = Some("party.code".into());
    d.naming = Some("INV-.YYYY.-.#####".into());
    d.fields = vec![
        number,
        of(FieldType::Link, "party", Some("party")),
        party_name,
        posting_date,
        due_date,
        status,
        of(FieldType::Table, "lines", Some("invoice_line")),
        of(FieldType::TableMultiselect, "picks", Some("pick")),
        total,
        remarks,
        tag,
        of(FieldType::Json, "extra", None),
        of(FieldType::Bytes, "data", None),
        of(FieldType::Attach, "attachment", None),
        of(FieldType::Secret, "secret_key", None),
    ];
    d
}

fn party() -> Definition {
    let mut d = Definition::new("party");
    d.title_field = Some("code".into());
    let mut code = text("code");
    code.required = true;
    code.unique = true;
    let mut active = of(FieldType::Bool, "active", None);
    active.default = Some("true".into());
    let mut since = of(FieldType::DateTime, "since", None);
    since.default = Some("2026-01-31T09:30:00Z".into());
    let mut note = text("note");
    note.default = Some("hello".into());
    let mut balance = of(FieldType::Decimal, "balance", None);
    balance.scale = Some(2);
    balance.precision = Some(6);
    balance.required = true;
    let mut start = of(FieldType::Decimal, "start", None);
    start.scale = Some(2);
    start.precision = Some(4);
    start.default = Some("125.50".into());
    let mut when = of(FieldType::Time, "when", None);
    when.default = Some("09:30".into());
    let mut day = of(FieldType::Date, "day", None);
    day.default = Some("2026-01-31".into());
    let mut weight = of(FieldType::Int, "weight", None);
    weight.default = Some("12".into());
    d.fields = vec![
        code,
        active,
        text("about_type"),
        of(FieldType::DynamicLink, "about", Some("about_type")),
        since,
        note,
        balance,
        start,
        when,
        day,
        weight,
    ];
    d
}

fn invoice_line() -> Definition {
    let mut d = Definition::new("invoice_line");
    d.is_child = true;
    let mut qty = of(FieldType::Decimal, "qty", None);
    qty.scale = Some(3);
    qty.required = true;
    qty.default = Some("1".into());
    d.fields = vec![set_required(text("item")), qty];
    d
}

fn pick() -> Definition {
    let mut d = Definition::new("pick");
    d.is_child = true;
    d.fields = vec![of(FieldType::Link, "ref_", Some("invoice"))];
    d
}

pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

pub fn of(ty: FieldType, name: &str, options: Option<&str>) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.options = options.map(str::to_owned);
    f
}

fn set_required(mut f: FieldSpec) -> FieldSpec {
    f.required = true;
    f
}

/// Builds the fixture set through the registry (customisations empty).
pub fn registry() -> Registry {
    Registry::new(definitions()).unwrap_or_else(|e| panic!("fixtures do not build: {e:?}"))
}

/// The fixture set's effective metadata.
pub fn effective() -> Arc<EffectiveMeta> {
    registry().effective()
}

/// The effective metadata of a custom definition set.
pub fn effective_from(defs: Vec<Definition>) -> Arc<EffectiveMeta> {
    Registry::new(defs)
        .unwrap_or_else(|e| panic!("fixtures do not build: {e:?}"))
        .effective()
}

/// A map's value by key name (`Name` is not `Borrow<str>`, so look up by iteration).
pub fn target<'m>(map: &'m BTreeMap<Name, Name>, key: &str) -> Option<&'m str> {
    map.iter()
        .find(|(k, _)| k.as_str() == key)
        .map(|(_, v)| v.as_str())
}

/// A parsed decimal by text, for tests.
pub fn decimal(text: &str) -> Decimal {
    Decimal::from_str_exact(text)
        .or_else(|_| Decimal::from_scientific(text))
        .unwrap_or_else(|e| panic!("test decimal `{text}`: {e}"))
}

/// A stored text value.
pub fn v_text(text: &str) -> Value {
    Value::Text(text.to_owned())
}

/// The kinds of the metadata errors, for matching.
pub fn kinds(errors: &[MetaError]) -> Vec<spot_meta::MetaErrorKind> {
    errors.iter().map(|e| e.kind).collect()
}
