//! The real sources of facts: an app manifest (methods and routes) and the effective metadata

mod support;

use spot_manifest::parse;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_ui_ext::{AppContribution, AppFacts, codes, validate};

const MANIFEST: &str = r#"
[app]
name = "sample"
version = "1.0.0"
title = "Sample"

[[methods]]
doctype = "task"
name = "start_work"
mutates = true

[[routes]]
path = "/sample/reports"
step = "sample:reports"
"#;

fn facts() -> AppFacts {
    let mut definition = Definition::new("task");
    definition
        .fields
        .push(FieldSpec::new("status", FieldType::Text));
    definition
        .fields
        .push(FieldSpec::new("assignee", FieldType::Text));
    let meta = Registry::build(vec![definition], &Customisations::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let manifest = parse(MANIFEST, "spot-app.toml")
        .unwrap_or_else(|errors| panic!("the sample manifest must parse: {errors:?}"));
    AppFacts::new()
        .with_manifest(&manifest)
        .with_meta(&meta)
        .with_destination("platform.home")
}

fn contribution(visible_if: &str) -> AppContribution {
    let document = format!(
        r#"{{
            "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample",
            "actions": [{{
                "id": "sample.start_work", "origin": "app:sample", "label": "Start work",
                "icon": "play", "slot": "doc.toolbar", "doctype": "task",
                "visible_if": "{visible_if}",
                "call": {{ "doctype": "task", "method": "start_work" }}
            }}]
        }}"#
    );
    AppContribution::new("sample", "1.0.0")
        .with_document("ui.json", document)
        .with_facts(facts())
}

#[test]
fn the_manifest_and_the_metadata_supply_the_facts() {
    let facts = facts();
    assert!(facts.method("task", "start_work").is_some());
    assert!(facts.method("task", "forget_everything").is_none());
    assert!(facts.route_covers("/sample/reports/summary"));
    assert!(!facts.route_covers("/other/page"));
    let task = facts
        .doctype("task")
        .unwrap_or_else(|| panic!("task is defined"));
    assert_eq!(task.fields, vec!["status", "assignee"]);
}

#[test]
fn an_expression_reading_a_real_field_validates_but_an_unknown_one_does_not() {
    assert!(
        validate(&contribution(
            "doc.status == 'Open' and not is_empty(doc.assignee)"
        ))
        .is_empty()
    );
    let findings = validate(&contribution("doc.statuz == 'Open'"));
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == codes::EXPRESSION_INVALID)
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.message.contains("doc.statuz"))
    );
}
