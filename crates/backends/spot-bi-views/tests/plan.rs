//! The generator, without a database: what is exposed, what is refused, what the SQL says.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_bi_views::{BiPlan, ExclusionReason, Protection, generate};
use spot_storage_contract::{ChildTableDef, EntityDef, EntityKind, FieldType, Name};
use support::*;

const GOLDEN: &str = include_str!("golden/fixture.sql");

fn plan_for(policy_text: &str) -> Result<BiPlan, spot_bi_views::BiErrors> {
    generate(
        &fixture_catalog(7),
        &policy(policy_text),
        &fixture_protection(),
    )
}

fn all_sql(plan: &BiPlan) -> String {
    let mut text = String::new();
    for view in plan.views.iter().chain(std::iter::once(&plan.catalogue)) {
        text.push_str(&view.create_sql(false));
        text.push('\n');
    }
    text
}

fn view<'a>(plan: &'a BiPlan, name: &str) -> &'a spot_bi_views::ViewPlan {
    plan.views
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no view {name}"))
}

#[test]
fn nothing_is_exposed_without_a_policy_entry() {
    let plan = plan_for("").unwrap();
    assert!(plan.views.is_empty());
    assert!(plan.catalogue.body.contains("WHERE false"));
}

#[test]
fn the_fixture_exposes_documents_and_children_with_the_expected_names() {
    let plan = fixture_plan();
    let names: Vec<&str> = plan.views.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "v1_party",
            "v1_sales_invoice",
            "v1_sales_invoice__sales_invoice_line"
        ]
    );
    assert_eq!(plan.catalog_version, 7);
}

#[test]
fn protected_secret_policy_excluded_and_unsupported_fields_are_reported_as_data() {
    let plan = fixture_plan();
    let reasons: Vec<(String, ExclusionReason)> = plan
        .excluded
        .iter()
        .map(|e| (e.column.clone(), e.reason.clone()))
        .collect();
    assert!(reasons.contains(&(
        "margin".into(),
        ExclusionReason::ProtectionGroup("cost".into())
    )));
    assert!(reasons.contains(&("api_token".into(), ExclusionReason::Secret)));
    assert!(reasons.contains(&(
        "tax_id".into(),
        ExclusionReason::ProtectionGroup("pii".into())
    )));
    assert!(reasons.contains(&("remarks".into(), ExclusionReason::PolicyExcluded)));
    assert!(reasons.contains(&(
        "flags".into(),
        ExclusionReason::UnsupportedType("json".into())
    )));
    assert!(reasons.contains(&("parent_field".into(), ExclusionReason::SystemInternal)));
}

#[test]
fn protected_fields_never_reach_any_view_or_the_catalogue() {
    let plan = fixture_plan();
    let sql = all_sql(&plan);
    for hidden in [
        "margin",
        "api_token",
        "tax_id",
        "remarks",
        "flags",
        "parent_field",
    ] {
        assert!(
            !sql.contains(&format!("\"{hidden}\"")),
            "`{hidden}` reached the SQL:\n{sql}"
        );
        for view in &plan.views {
            assert!(
                view.columns.iter().all(|c| c.name != hidden),
                "`{hidden}` is a column of {}",
                view.name
            );
        }
    }
}

#[test]
fn links_show_the_target_title_and_the_child_view_follows_the_parent() {
    let plan = fixture_plan();
    let invoice = view(&plan, "v1_sales_invoice");
    let name_column = invoice
        .columns
        .iter()
        .find(|c| c.name == "customer_name")
        .expect("link name column");
    assert_eq!(name_column.pg_type, "text");
    assert!(
        invoice
            .body
            .contains("LEFT JOIN \"public\".\"party\" AS \"l0\"")
    );
    assert!(invoice.body.contains("WHERE \"t\".\"docstatus\" = 1"));
    let child = view(&plan, "v1_sales_invoice__sales_invoice_line");
    assert!(
        child
            .body
            .contains("JOIN \"public\".\"sales_invoice\" AS \"p\"")
    );
    assert!(child.body.contains("WHERE \"p\".\"docstatus\" = 1"));
    assert_eq!(child.parent.as_deref(), Some("sales_invoice"));
}

#[test]
fn typed_columns_use_the_contract_types() {
    let plan = fixture_plan();
    let types = |name: &str| -> Vec<(String, String)> {
        view(&plan, name)
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.pg_type.clone()))
            .collect()
    };
    let invoice = types("v1_sales_invoice");
    for (column, ty) in [
        ("total", "numeric(18,2)"),
        ("posting_date", "date"),
        ("seen_at", "timestamp with time zone"),
        ("at_time", "time without time zone"),
        ("is_hold", "boolean"),
        ("customer", "uuid"),
        ("docstatus", "bigint"),
    ] {
        assert!(
            invoice.contains(&(column.to_owned(), ty.to_owned())),
            "{column} should be {ty}: {invoice:?}"
        );
    }
    let line = types("v1_sales_invoice__sales_invoice_line");
    assert!(line.contains(&("qty".to_owned(), "numeric(18,3)".to_owned())));
}

#[test]
fn draft_rows_are_filtered_only_when_the_policy_says_posted_only() {
    let plan = plan_for(
        r#"expose = ["party", "sales_invoice"]
[entity.party]
posted_only = false
[entity.sales_invoice]
posted_only = false
"#,
    )
    .unwrap();
    let invoice = view(&plan, "v1_sales_invoice");
    assert!(!invoice.body.contains("WHERE"));
}

#[test]
fn a_child_view_needs_the_parent_to_expose_children() {
    let plan = plan_for(
        r#"expose = ["party", "sales_invoice"]
[entity.party]
posted_only = false
[entity.sales_invoice]
children = false
"#,
    )
    .unwrap();
    assert_eq!(plan.views.len(), 2);
}

#[test]
fn every_problem_is_reported_in_one_pass() {
    let policy_text = r#"
expose = ["party", "sales_invoice", "ghost", "sales_invoice_line"]
[entity.party]
include = ["tax_id", "nope"]
title_field = "tax_id"
[entity.sales_invoice]
include = ["margin", "flags", "api_token", "missing"]
exclude = ["id"]
[entity.sales_invoice_line]
posted_only = false
"#;
    let errors = plan_for(policy_text).unwrap_err();
    let codes = errors.codes();
    for code in [
        "bi.unknown_entity",
        "bi.policy_invalid",
        "bi.protected_column",
        "bi.unknown_column",
        "bi.unsupported_type",
    ] {
        assert!(codes.contains(&code), "missing {code}: {errors}");
    }
    assert!(codes.len() >= 8, "{errors}");
}

#[test]
fn a_protected_title_field_cannot_show_names() {
    let errors = plan_for(
        r#"expose = ["party", "sales_invoice"]
[entity.party]
posted_only = false
title_field = "tax_id"
"#,
    )
    .unwrap_err();
    assert!(errors.codes().contains(&"bi.policy_invalid"), "{errors}");
}

#[test]
fn a_protected_posted_marker_is_refused() {
    let protection = Protection::new()
        .group_field(
            "sales_invoice",
            "docstatus",
            &spot_permissions::GroupName("x".into()),
        )
        .unwrap();
    let errors = generate(
        &fixture_catalog(7),
        &policy(
            r#"expose = ["sales_invoice"]
[entity.sales_invoice]
children = false
"#,
        ),
        &protection,
    )
    .unwrap_err();
    assert_eq!(errors.codes(), vec!["bi.posted_marker"]);
}

#[test]
fn a_document_without_a_posted_marker_needs_posted_only_off() {
    let mut entities = fixture_entities();
    entities.retain(|e| e.name.as_str() != "sales_invoice_line");
    let invoice = entities
        .iter_mut()
        .find(|e| e.name.as_str() == "sales_invoice")
        .unwrap();
    invoice.fields.retain(|f| f.name.as_str() != "docstatus");
    invoice.children.clear();
    let catalog = catalog_of(1, entities);
    let errors = generate(
        &catalog,
        &policy(r#"expose = ["sales_invoice"]"#),
        &Protection::new(),
    )
    .unwrap_err();
    assert_eq!(errors.codes(), vec!["bi.posted_marker"]);
    let ok = generate(
        &catalog,
        &policy(
            r#"expose = ["sales_invoice"]
[entity.sales_invoice]
posted_only = false
"#,
        ),
        &Protection::new(),
    );
    assert!(ok.is_ok());
}

#[test]
fn a_name_longer_than_the_identifier_limit_is_refused() {
    let doc = "d".repeat(40);
    let child = "c".repeat(20);
    let mut entities = vec![
        EntityDef {
            name: n(&doc),
            kind: EntityKind::Document,
            fields: vec![field("docstatus", FieldType::Int, false)],
            children: vec![ChildTableDef {
                field: n("rows"),
                entity: n(&child),
            }],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
        EntityDef {
            name: n(&child),
            kind: EntityKind::Child { parent: n(&doc) },
            fields: vec![],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
    ];
    let catalog = catalog_of(1, entities.clone());
    let text = format!("expose = [\"{doc}\"]\n[entity.{doc}]\nchildren = true\n");
    let errors = generate(&catalog, &policy(&text), &Protection::new()).unwrap_err();
    assert_eq!(errors.codes(), vec!["bi.name_too_long"]);
    entities.clear();
}

#[test]
fn two_names_that_become_one_view_are_refused() {
    // Document `a` with child `b` gives `v1_a__b`; document `a__b` gives the same name.
    let child = EntityDef {
        name: n("b"),
        kind: EntityKind::Child { parent: n("a") },
        fields: vec![],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let a = EntityDef {
        name: n("a"),
        kind: EntityKind::Document,
        fields: vec![field("docstatus", FieldType::Int, false)],
        children: vec![ChildTableDef {
            field: n("rows"),
            entity: n("b"),
        }],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let ab = EntityDef {
        name: n("a__b"),
        kind: EntityKind::Document,
        fields: vec![field("docstatus", FieldType::Int, false)],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let catalog = catalog_of(1, vec![a, child, ab]);
    let errors = generate(
        &catalog,
        &policy("expose = [\"a\", \"a__b\"]\n[entity.a]\nchildren = true\n"),
        &Protection::new(),
    )
    .unwrap_err();
    assert_eq!(errors.codes(), vec!["bi.name_collision"]);
}

#[test]
fn a_link_name_that_collides_with_a_field_is_refused() {
    let mut entities = fixture_entities();
    let invoice = entities
        .iter_mut()
        .find(|e| e.name.as_str() == "sales_invoice")
        .unwrap();
    invoice.fields.push(field("customer_name", text(), true));
    let errors = generate(
        &catalog_of(1, entities),
        &fixture_policy(),
        &fixture_protection(),
    )
    .unwrap_err();
    assert!(errors.codes().contains(&"bi.name_collision"), "{errors}");
}

#[test]
fn reserved_words_and_long_names_are_quoted_and_still_valid_names() {
    let mut names: Vec<String> = [
        "order", "select", "user", "table", "group", "all", "bi", "x__y",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    names.push("a".repeat(40));
    // A deterministic walk over the storage-name alphabet.
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789_".chars().collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..200 {
        let mut name = String::from("a");
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let len = 1 + (seed >> 33) as usize % 40;
        for _ in 0..len {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            name.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
        }
        names.push(name);
    }
    for name in names {
        if name.starts_with("spot_") {
            continue;
        }
        let entities = vec![EntityDef {
            name: n(&name),
            kind: EntityKind::Document,
            fields: vec![
                field("docstatus", FieldType::Int, false),
                field("select", text(), true),
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        }];
        let plan = generate(
            &catalog_of(1, entities),
            &policy(&format!("expose = [\"{name}\"]\n")),
            &Protection::new(),
        );
        let plan = plan.unwrap_or_else(|e| panic!("{name}: {e}"));
        let body = &plan.views[0].body;
        assert!(
            body.contains(&format!("\"{name}\"")),
            "{name} not quoted: {body}"
        );
        assert!(body.contains("\"select\""), "{body}");
        assert_eq!(
            body.matches('"').count() % 2,
            0,
            "unbalanced quotes: {body}"
        );
    }
}

#[test]
fn identifiers_that_try_to_break_out_are_refused_by_the_name_type() {
    for hostile in [
        "x\"; DROP TABLE party; --",
        "x' OR '1'='1",
        "Upper",
        "1start",
        "has space",
        "dot.name",
        "",
    ] {
        assert!(
            Name::new(hostile).is_err(),
            "`{hostile}` must not be a name"
        );
    }
}

#[test]
fn literal_text_from_the_policy_is_escaped_and_cannot_end_the_string() {
    let plan = fixture_plan();
    let catalogue = &plan.catalogue.body;
    // The label contains a quote and a backslash; both must be escaped in the E'' literal.
    assert!(catalogue.contains("E'it''s a \\\\ test'"), "{catalogue}");
    assert!(!catalogue.contains("it's"), "{catalogue}");
}

#[test]
fn the_catalogue_lists_every_exposed_column_and_no_hidden_one() {
    let plan = fixture_plan();
    let rows = &plan.catalogue.body;
    assert!(rows.contains("'bi.v1_sales_invoice'"));
    assert!(rows.contains("'customer_name'"));
    assert!(!rows.contains("'margin'"));
    assert!(!rows.contains("'api_token'"));
    assert!(!rows.contains("'tax_id'"));
}

#[test]
fn the_views_match_the_golden_sql() {
    let actual = all_sql(&fixture_plan());
    if std::env::var("SPOT_BI_UPDATE_GOLDEN").as_deref() == Ok("1") {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/fixture.sql");
        std::fs::write(path, &actual).unwrap();
        return;
    }
    assert_eq!(
        actual, GOLDEN,
        "generated SQL changed; review it, then rerun with SPOT_BI_UPDATE_GOLDEN=1"
    );
}

fn fixture_plan() -> BiPlan {
    generate(
        &fixture_catalog(7),
        &fixture_policy(),
        &fixture_protection(),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}
