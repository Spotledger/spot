//! The compiled plan: the dense field index (declaration order, engine-managed fields
//! included), the per-field data, the link/table/dynamic-link maps, `check_value` and the
//! plan hash.
#![allow(missing_docs)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{decimal, effective, registry, text};
use spot_app_api::meta::{AttachFormat, DecimalFormat, FieldDef, FieldKind, IntFormat};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_meta::{
    Customisations, Definition, EffectiveMeta, FieldType, MetaErrorKind as K, Registry,
};
use spot_plan::{DocPlan, FieldIx, PlanError, PlanSet};
use std::sync::Arc;

/// The party plan, for field-by-field value checks.
fn plan_for(doctype: &str) -> Arc<DocPlan> {
    PlanSet::build(&effective())
        .unwrap_or_else(|e| panic!("fixtures do not compile: {e:?}"))
        .get(doctype)
        .unwrap_or_else(|| panic!("no {doctype} plan"))
}

#[test]
fn the_index_is_dense_in_declaration_order_including_engine_fields() {
    let plan = plan_for("invoice");
    let names: Vec<&str> = plan.fields().iter().map(|f| f.name.as_str()).collect();
    // Declaration order; the engine-managed fields are appended last on a posted DocType.
    assert_eq!(
        names,
        [
            "number",
            "party",
            "party_name",
            "posting_date",
            "due_date",
            "status",
            "lines",
            "picks",
            "total",
            "remarks",
            "tag",
            "extra",
            "data",
            "attachment",
            "secret_key",
            "doc_state",
            "amended_from",
        ]
    );
    for (ix, name) in names.iter().enumerate() {
        assert_eq!(plan.field_ix(name), Some(FieldIx::new(ix)), "`{name}`");
        assert_eq!(plan.field(name).map(|f| f.name.as_str()), Some(*name));
    }
    assert_eq!(plan.field_ix("nope"), None);
    assert!(plan.field("nope").is_none());
}

#[test]
fn per_field_data_carries_the_rules_and_the_constraints() {
    let plan = plan_for("invoice");
    let total = plan.field("total").unwrap_or_else(|| panic!("no total"));
    // From the definition (the app view carries none of these flags).
    assert!(!total.required && !total.unique);
    assert!(total.read_only, "total is read-only in the fixture");
    // The kind is the app view's, with its parsed constraints.
    assert!(matches!(
        total.kind,
        FieldKind::Decimal {
            scale: 2,
            precision: 18,
            format: DecimalFormat::Plain
        }
    ));
    let status = plan.field("status").unwrap_or_else(|| panic!("no status"));
    assert!(
        status.allow_on_posted,
        "status may change on a posted document"
    );
    assert_eq!(
        status.options,
        ["Open", "Paid"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "select options as a sorted set, whatever the declaration order was"
    );
    assert_eq!(
        status.default,
        Some(Value::Text("Open".into())),
        "the default is the parsed value already in the app view"
    );
    let amended = plan
        .field("amended_from")
        .unwrap_or_else(|| panic!("no amended_from"));
    assert!(amended.read_only, "the engine-managed fields are read-only");
    assert!(matches!(amended.kind, FieldKind::Link { .. }));
}

#[test]
fn the_maps_name_every_target() {
    let party = plan_for("party");
    // A dynamic link: field -> the field naming its target.
    assert_eq!(target(party.dynamic_links(), "about"), Some("about_type"));
    assert!(party.links().is_empty(), "party has no link fields");

    let invoice = plan_for("invoice");
    assert_eq!(target(invoice.links(), "party"), Some("party"));
    // The engine-managed amended_from is a real link, to the DocType itself.
    assert_eq!(target(invoice.links(), "amended_from"), Some("invoice"));
    assert_eq!(target(invoice.tables(), "lines"), Some("invoice_line"));
    assert_eq!(target(invoice.tables(), "picks"), Some("pick"));
    assert!(invoice.dynamic_links().is_empty());

    let pick = plan_for("pick");
    assert!(
        matches!(&pick.field("ref_").unwrap_or_else(|| panic!("no ref_")).kind,
        FieldKind::Link { doctype } if doctype.as_str() == "invoice")
    );
}

/// A map's value by key name (`Name` is not `Borrow<str>`, so look up by iteration).
fn target<'m>(map: &'m BTreeMap<Name, Name>, key: &str) -> Option<&'m str> {
    map.iter()
        .find(|(k, _)| k.as_str() == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn check_value_by_kind() {
    let party = plan_for("party");
    assert!(party.check_value("active", &Value::Bool(true)).is_ok());
    assert_kind(&party.check_value("active", &Value::Int(1)), "active");
    assert!(party.check_value("weight", &Value::Int(-3)).is_ok());
    assert_kind(&party.check_value("weight", &Value::Bool(false)), "weight");
    // `Null` fits every scalar kind.
    assert!(party.check_value("note", &Value::Null).is_ok());

    let invoice = plan_for("invoice");
    // A link accepts an id.
    assert!(
        invoice
            .check_value("party", &Value::Id(RecordId::new_v7()))
            .is_ok()
    );
    assert_kind(
        &invoice.check_value("party", &Value::Text("x".into())),
        "party",
    );
    // A child table accepts nothing (its rows are checked against the child DocType).
    assert_kind(&invoice.check_value("lines", &Value::Null), "lines");
    // A secret accepts only null: the value never travels in a document (ADR-068 §2).
    assert!(invoice.check_value("secret_key", &Value::Null).is_ok());
    assert_kind(
        &invoice.check_value("secret_key", &Value::Text("x".into())),
        "secret_key",
    );
    assert!(
        invoice
            .check_value("data", &Value::Bytes(vec![1, 2]))
            .is_ok()
    );
    assert_kind(
        &invoice.check_value("data", &Value::Text("x".into())),
        "data",
    );
    assert!(
        invoice
            .check_value("extra", &Value::Json("{}".into()))
            .is_ok()
    );
}

fn assert_kind(result: &Result<(), PlanError>, field: &str) {
    match result {
        Err(PlanError::KindMismatch { field: f, got, .. }) => {
            assert_eq!(f, field);
            assert!(!got.is_empty());
        }
        other => panic!("expected plan.value.kind for `{field}`, got {other:?}"),
    }
}

#[test]
fn the_unknown_field_is_reported_never_panics() {
    let plan = plan_for("party");
    let err = plan.check_value("totally_unknown", &Value::Null);
    match &err {
        Err(PlanError::UnknownField { doctype, field }) => {
            assert_eq!(
                (doctype.as_str(), field.as_str()),
                ("party", "totally_unknown")
            );
        }
        other => panic!("expected plan.field.unknown, got {other:?}"),
    }
    assert_eq!(err.err().map(|e| e.code()), Some("plan.field.unknown"));
}

#[test]
fn max_chars_counts_characters() {
    let plan = plan_for("invoice");
    assert!(
        plan.check_value("tag", &Value::Text("12345".into()))
            .is_ok()
    );
    let too_long = plan.check_value("tag", &Value::Text("123456".into()));
    match &too_long {
        Err(PlanError::TooLong { max, .. }) => assert_eq!(max, &5),
        other => panic!("expected plan.value.too_long, got {other:?}"),
    }
    assert_eq!(
        too_long.err().map(|e| e.code()),
        Some("plan.value.too_long")
    );
    // Six characters of `é` are twelve bytes but six characters.
    assert!(
        plan.check_value("tag", &Value::Text("éééééé".into()))
            .is_err()
    );
    // Fields without a limit take any length.
    assert!(
        plan.check_value("remarks", &Value::Text("x".repeat(10_000)))
            .is_ok()
    );
}

#[test]
fn decimals_reject_over_scale_and_overflow_never_round() {
    let invoice = plan_for("invoice");
    assert!(
        invoice
            .check_value("total", &Value::Decimal(decimal("12.55")))
            .is_ok()
    );
    // An over-scale whose extra digits are all trailing zeros passes (the storage rule).
    assert!(
        invoice
            .check_value("total", &Value::Decimal(decimal("12.550")))
            .is_ok()
    );
    let too_precise = invoice.check_value("total", &Value::Decimal(decimal("12.553")));
    assert_eq!(
        too_precise.err().map(|e| e.code()),
        Some("plan.value.scale")
    );
    // total is precision 18 scale 2, so at most 16 integer digits.
    assert!(
        invoice
            .check_value("total", &Value::Decimal(decimal("1234567890123456.78")))
            .is_ok()
    );
    let overflow = invoice.check_value("total", &Value::Decimal(decimal("12345678901234567.78")));
    assert_eq!(
        overflow.err().map(|e| e.code()),
        Some("plan.value.precision")
    );

    // party.balance is precision 6 scale 2: at most 4 integer digits.
    let party = plan_for("party");
    assert!(
        party
            .check_value("balance", &Value::Decimal(decimal("1234.99")))
            .is_ok()
    );
    assert!(
        party
            .check_value("balance", &Value::Decimal(decimal("12345.99")))
            .is_err()
    );
    // A default the metadata parsed carries no over-scale by construction.
    let start = party.field("start").unwrap_or_else(|| panic!("no start"));
    assert_eq!(start.default, Some(Value::Decimal(decimal("125.50"))));
}

#[test]
fn select_membership() {
    let plan = plan_for("invoice");
    assert!(
        plan.check_value("status", &Value::Text("Paid".into()))
            .is_ok()
    );
    let not_an = plan.check_value("status", &Value::Text("Refunded".into()));
    match &not_an {
        Err(PlanError::NotAnOption { value, .. }) => assert_eq!(value, "Refunded"),
        other => panic!("expected plan.value.not_an_option, got {other:?}"),
    }
    assert_eq!(
        not_an.err().map(|e| e.code()),
        Some("plan.value.not_an_option")
    );
    assert!(plan.check_value("status", &Value::Null).is_ok());
}

/// The plan hash per DocType, from one build.
fn plan_hashes(e: &EffectiveMeta) -> BTreeMap<String, String> {
    let set = PlanSet::build(e).unwrap_or_else(|err| panic!("plans do not compile: {err:?}"));
    set.doctypes()
        .map(|name| {
            (
                name.to_owned(),
                set.get(name)
                    .unwrap_or_else(|| panic!("no {name} plan"))
                    .plan_hash()
                    .as_hex()
                    .to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_plan_hash_is_stable_across_two_builds() {
    assert_eq!(
        plan_hashes(&common::effective()),
        plan_hashes(&registry().effective()),
        "two builds of the same metadata are provably identical"
    );
}

#[test]
fn every_plan_hash_is_64_lowercase_hex_and_distinct_between_doctypes() {
    let set =
        PlanSet::build(&effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    let mut hexes = Vec::new();
    for name in set.doctypes() {
        let plan = set.get(name).unwrap_or_else(|| panic!("no {name} plan"));
        let hash = plan.plan_hash().as_hex();
        assert_eq!(hash.len(), 64, "{name}");
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name}: {hash}"
        );
        hexes.push(hash.to_owned());
    }
    let distinct: BTreeSet<&String> = hexes.iter().collect();
    assert_eq!(
        distinct.len(),
        hexes.len(),
        "distinct plans, distinct hashes"
    );
}

#[test]
fn a_changed_metadata_changes_the_hash_of_exactly_one_doctype() {
    let mut reg = registry();
    let before = plan_hashes(&reg.effective());
    reg.add_custom_field("party", text("custom_score"), None)
        .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
    let after = plan_hashes(&reg.effective());
    assert_eq!(before.len(), after.len());
    let changed: Vec<&String> = before
        .keys()
        .filter(|name| before[*name] != after[*name])
        .collect();
    assert_eq!(changed, ["party"], "only the changed DocType's hash moves");
}

#[test]
fn a_custom_field_appears_in_the_new_plan_at_its_place() {
    let mut reg = registry();
    let set =
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    assert!(
        set.get("party")
            .unwrap_or_else(|| panic!("no party"))
            .field("custom_score")
            .is_none()
    );
    reg.add_custom_field("party", text("custom_score"), Some("code"))
        .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
    let set2 =
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    let party = set2.get("party").unwrap_or_else(|| panic!("no party"));
    assert_eq!(
        party.field_ix("custom_score"),
        Some(FieldIx::new(1)),
        "after `code`"
    );
    assert_eq!(
        party.field_ix("about_type"),
        Some(FieldIx::new(3)),
        "the rest shifts"
    );
    assert_eq!(set2.meta_version(), set.meta_version() + 1);
}

#[test]
fn a_bad_default_fails_the_metadata_build_naming_the_field() {
    let mut defs = common::definitions();
    let mut d = Definition::new("bad_default_dt");
    let mut f = text("note");
    f.max_chars = Some(5);
    f.default = Some("way too long".into());
    d.fields = vec![f];
    defs.push(d);
    let errors = Registry::build(defs, &Customisations::new())
        .err()
        .unwrap_or_else(|| panic!("the bad default built a metadata"));
    assert_eq!(common::kinds(&errors), [K::BadDefault]);
    let e = errors.first().unwrap_or_else(|| panic!("one error"));
    assert_eq!(
        (e.doctype.as_deref(), e.field.as_deref()),
        (Some("bad_default_dt"), Some("note"))
    );

    // The other default rules are compile errors too: a select default off the list.
    let mut defs = common::definitions();
    let mut d = Definition::new("bad_select_dt");
    let mut f = common::of(FieldType::Select, "state", Some("Open\nPaid"));
    f.default = Some("Refunded".into());
    d.fields = vec![f];
    defs.push(d);
    let errors = Registry::build(defs, &Customisations::new())
        .err()
        .unwrap_or_else(|| panic!("the bad select default built a metadata"));
    assert_eq!(common::kinds(&errors), [K::BadDefault]);
    assert_eq!(
        errors.first().and_then(|e| e.field.as_deref()),
        Some("state")
    );
}

#[test]
fn the_plan_compile_refuses_a_default_the_view_cannot_carry() {
    let meta = effective();
    let def = meta
        .definition("party")
        .map(Definition::to_owned)
        .unwrap_or_else(|| panic!("no party"));
    let view = meta
        .doctype("party")
        .unwrap_or_else(|| panic!("no party view"));
    // A view stripped of every parsed default: the malformed-default signal.
    let stripped = DocTypeDefCopy::from(&view);
    let errors = DocPlan::compile(&def, &stripped.def)
        .err()
        .unwrap_or_else(|| panic!("the dropped default compiled"));
    assert_eq!(
        errors.len(),
        7,
        "every defaulted field of the party reports at once"
    );
    assert!(
        errors
            .iter()
            .all(|err| err.code() == "plan.compile.bad_default")
    );
    let named: Vec<&str> = errors.iter().filter_map(PlanError::field).collect();
    for field in ["active", "since", "note", "start", "when", "day", "weight"] {
        assert!(
            named.contains(&field),
            "each error names the field: {errors:?}"
        );
    }
}

/// An owned copy of a `DocTypeDef` whose defaults are stripped.
struct DocTypeDefCopy {
    def: spot_app_api::meta::DocTypeDef,
}

impl DocTypeDefCopy {
    fn from(view: &spot_app_api::meta::DocTypeDef) -> Self {
        Self {
            def: rebuild(view, |f| {
                FieldDef::new(
                    f.name.clone(),
                    f.kind.clone(),
                    f.required,
                    f.allow_on_posted,
                )
            }),
        }
    }
}

/// Rebuilds a `DocTypeDef` field by field (the type is `#[non_exhaustive]`).
fn rebuild(
    view: &spot_app_api::meta::DocTypeDef,
    map: impl Fn(&FieldDef) -> FieldDef,
) -> spot_app_api::meta::DocTypeDef {
    let mut out = spot_app_api::meta::DocTypeDef::new(
        view.name.clone(),
        view.label.clone(),
        view.is_posted,
        view.is_child,
        view.title_field.clone(),
        view.fields.iter().map(map).collect(),
    );
    out.version = view.version;
    out
}

#[test]
fn the_plan_compile_refuses_a_mismatched_pair() {
    let meta = effective();
    let def = meta
        .definition("party")
        .map(Definition::to_owned)
        .unwrap_or_else(|| panic!("no party"));
    let other = meta
        .doctype("invoice")
        .unwrap_or_else(|| panic!("no invoice"));
    let errors = DocPlan::compile(&def, &other)
        .err()
        .unwrap_or_else(|| panic!("compiled"));
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .all(|err| err.code() == "plan.compile.view_mismatch")
    );
}

#[test]
fn the_error_codes_are_stable_and_distinct() {
    let meta = effective();
    let def = meta
        .definition("invoice")
        .map(Definition::to_owned)
        .unwrap_or_else(|| panic!("no invoice"));
    let view = meta.doctype("invoice").unwrap_or_else(|| panic!("no view"));
    let plan =
        DocPlan::compile(&def, &view).unwrap_or_else(|err| panic!("does not compile: {err:?}"));
    let mut seen = BTreeSet::new();
    for (result, code) in [
        (plan.check_value("nope", &Value::Null), "plan.field.unknown"),
        (plan.check_value("lines", &Value::Null), "plan.value.kind"),
        (
            plan.check_value("tag", &Value::Text("123456".into())),
            "plan.value.too_long",
        ),
        (
            plan.check_value("total", &Value::Decimal(decimal("12.553"))),
            "plan.value.scale",
        ),
        (
            plan.check_value("total", &Value::Decimal(decimal("12345678901234567.78"))),
            "plan.value.precision",
        ),
        (
            plan.check_value("status", &Value::Text("Nope".into())),
            "plan.value.not_an_option",
        ),
    ] {
        let error = result
            .err()
            .unwrap_or_else(|| panic!("expected the error {code}"));
        assert_eq!(error.code(), code, "{error:?}");
        // Every value error names its field and DocType.
        assert_eq!(error.doctype(), "invoice");
        assert!(error.field().is_some(), "every value error names the field");
        assert!(seen.insert(code), "duplicate code {code}");
    }
    for (errors, code) in [
        (
            vec![PlanError::BadDefault {
                doctype: "d".into(),
                field: "f".into(),
                default: "x".into(),
                message: "m".into(),
            }],
            "plan.compile.bad_default",
        ),
        (
            vec![PlanError::ViewMismatch {
                doctype: "d".into(),
                message: "m".into(),
            }],
            "plan.compile.view_mismatch",
        ),
    ] {
        assert_eq!(errors[0].code(), code);
        assert_eq!(errors[0].doctype(), "d");
        assert!(seen.insert(code), "duplicate code {code}");
    }
    assert_eq!(seen.len(), 8, "every stable code is covered");
}

#[test]
fn the_kind_formats_travel_into_the_plan() {
    let plan = plan_for("invoice");
    let attachment = plan
        .field("attachment")
        .unwrap_or_else(|| panic!("no attachment"));
    assert!(matches!(
        attachment.kind,
        FieldKind::Attach {
            format: AttachFormat::File
        }
    ));
    let party = plan_for("party");
    let weight = party.field("weight").unwrap_or_else(|| panic!("no weight"));
    assert!(matches!(
        weight.kind,
        FieldKind::Int {
            format: IntFormat::Plain
        }
    ));
}

/// The name of this set's DocType count, so the fixture list is pinned by the tests.
#[test]
fn the_fixture_set_has_the_expected_doctypes() {
    let set =
        PlanSet::build(&effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    let expected: &str = "file, invoice, invoice_line, party, pick";
    let actual = set.doctypes().collect::<Vec<_>>().join(", ");
    assert_eq!(actual, expected);
}

/// The use of `use` here keeps `Name` and `Value` in one place for the tests above.
#[test]
fn decimal_defaults_parse_and_carry_their_scale() {
    let party = plan_for("party");
    let weight = party.field("weight").unwrap_or_else(|| panic!("no weight"));
    assert_eq!(weight.default, Some(Value::Int(12)));
    let day = party.field("day").unwrap_or_else(|| panic!("no day"));
    assert!(matches!(day.default, Some(Value::Date(_))));
    let since = party.field("since").unwrap_or_else(|| panic!("no since"));
    assert!(matches!(since.default, Some(Value::DateTime(_))));
    let when = party.field("when").unwrap_or_else(|| panic!("no when"));
    assert!(matches!(when.default, Some(Value::Time(_))));
}
