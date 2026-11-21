//! Compile-time refusals: every problem is reported at once, with the right `report.*` code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_reports::ReportError;
use support::{compile, world};

fn codes(errors: &[ReportError]) -> Vec<&str> {
    errors.iter().map(ReportError::code).collect()
}

fn base(extra: &str) -> String {
    format!(
        r#"
name = "probe"
label = "Probe"
base = "journal"
roles = ["reader"]
[[columns]]
field = "title"
{extra}
"#
    )
}

#[tokio::test]
async fn an_unknown_field_is_refused() {
    let world = world().await;
    let errs = compile(&world, &base("[[columns]]\nfield = \"missing\"\n")).unwrap_err();
    assert!(codes(&errs).contains(&"report.unknown_field"), "{errs:?}");
}

#[tokio::test]
async fn an_unknown_join_alias_is_refused() {
    let world = world().await;
    let errs = compile(&world, &base("[[columns]]\nfield = \"zz.title\"\n")).unwrap_err();
    assert!(codes(&errs).contains(&"report.unknown_join"), "{errs:?}");
}

#[tokio::test]
async fn an_unknown_parameter_is_refused() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "journal"
roles = ["reader"]
[[columns]]
field = "title"
[filters]
field = "title"
op = "eq"
param = "nope"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(
        codes(&errs).contains(&"report.unknown_parameter"),
        "{errs:?}"
    );
}

#[tokio::test]
async fn a_duplicate_column_is_refused() {
    let world = world().await;
    let errs = compile(&world, &base("[[columns]]\nfield = \"title\"\n")).unwrap_err();
    assert!(codes(&errs).contains(&"report.duplicate"), "{errs:?}");
}

#[tokio::test]
async fn a_wrong_parameter_type_is_refused() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "journal"
roles = ["reader"]
[[columns]]
field = "debit"
[[parameters]]
name = "word"
label = "Word"
type = "text"
required = true
[filters]
field = "debit"
op = "eq"
param = "word"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(codes(&errs).contains(&"report.bad_type"), "{errs:?}");
}

#[tokio::test]
async fn a_missing_base_is_refused() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "nope"
roles = ["reader"]
[[columns]]
field = "title"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(codes(&errs).contains(&"report.base_missing"), "{errs:?}");
}

#[tokio::test]
async fn a_limit_above_the_maximum_is_refused() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "journal"
limit = 500
max_rows = 100
roles = ["reader"]
[[columns]]
field = "title"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(
        codes(&errs).contains(&"report.invalid_definition"),
        "{errs:?}"
    );
}

#[tokio::test]
async fn too_many_joins_is_refused() {
    let world = world().await;
    let mut toml = String::from(
        "name = \"probe\"\nlabel = \"Probe\"\nbase = \"journal\"\nroles = [\"reader\"]\n",
    );
    for i in 0..17 {
        toml.push_str(&format!(
            "[[joins]]\nalias = \"a{i}\"\ndoctype = \"account\"\non = \"account\"\n"
        ));
    }
    toml.push_str("[[columns]]\nfield = \"title\"\n");
    let errs = compile(&world, &toml).unwrap_err();
    assert!(codes(&errs).contains(&"report.join_limit"), "{errs:?}");
}

#[tokio::test]
async fn a_too_deep_filter_is_refused() {
    let world = world().await;
    let mut node = "{ field = \"title\", op = \"eq\", value = \"x\" }".to_owned();
    for _ in 0..70 {
        node = format!("{{ not = {node} }}");
    }
    let toml = format!(
        "name = \"probe\"\nlabel = \"Probe\"\nbase = \"journal\"\nroles = [\"reader\"]\n\
         filters = {node}\n[[columns]]\nfield = \"title\"\n"
    );
    let errs = compile(&world, &toml).unwrap_err();
    assert!(codes(&errs).contains(&"report.too_large"), "{errs:?}");
}

#[tokio::test]
async fn a_too_large_expression_is_refused() {
    let world = world().await;
    let mut expr = "1".to_owned();
    for _ in 0..2001 {
        expr.push_str(" + 1");
    }
    let toml = format!(
        "name = \"probe\"\nlabel = \"Probe\"\nbase = \"journal\"\nroles = [\"reader\"]\n\
         [[columns]]\nfield = \"debit\"\n[[computed]]\nname = \"big\"\ntype = \"int\"\nexpr = \"{expr}\"\n"
    );
    let errs = compile(&world, &toml).unwrap_err();
    assert!(codes(&errs).contains(&"report.expression"), "{errs:?}");
}

#[tokio::test]
async fn an_ungrouped_column_is_refused() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "journal"
roles = ["reader"]
[[columns]]
field = "title"
[[columns]]
field = "debit"
aggregate = "sum"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(
        codes(&errs).contains(&"report.invalid_definition"),
        "{errs:?}"
    );
}

#[tokio::test]
async fn every_problem_is_reported_at_once() {
    let world = world().await;
    let toml = r#"
name = "probe"
label = "Probe"
base = "journal"
roles = ["reader"]
[[columns]]
field = "missing_one"
[[columns]]
field = "zz.missing_two"
"#;
    let errs = compile(&world, toml).unwrap_err();
    assert!(errs.len() >= 2, "both problems reported: {errs:?}");
    assert!(codes(&errs).contains(&"report.unknown_field"));
    assert!(codes(&errs).contains(&"report.unknown_join"));
}
