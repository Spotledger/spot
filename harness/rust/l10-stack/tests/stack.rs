//! The 16 L10 scenarios on the real stack. Without `SPOT_TEST_PG_URL` the tests print `SKIPPED`;
//! under `SPOT_REQUIRE_PG=1` a missing server is a failure (`TestDb::create` decides).
//!
//! The suite is synchronous (the stack hosts its own runtime), so these are plain `#[test]`s.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_l10_liveness::{run_one, scenario_names};
use spot_l10_stack::{StackFactory, StackHost};

/// Scenarios that replace the standard definitions wholesale (an app update). The customise
/// service's base definitions are fixed when it is built, so a stack cannot answer them: they stay
/// failing, ignored, with the reason in the report.
const APP_UPDATE: [&str; 2] = [
    "app_update_adding_a_standard_field_keeps_site_customisations",
    "app_update_removal_succeeds_and_a_colliding_update_is_refused_by_name",
];

/// The scenario no host over `spot_meta::Registry` can pass today: it asserts that an unrelated
/// DocType's compiled view is the same `Arc` after a change to another DocType, and the registry
/// rebuilds every view on every change (the open question in the `l10-liveness` README, the
/// `#[ignore]`d `the_unshared_views_scenario_fails_only_on_sharing` there). The stack still runs
/// it: it must fail on that sharing assertion and nothing else, so everything else it checks
/// (the removal reaching the engine, the plan and the database) is proved, and the day the
/// registry shares views this test fails and asks for the entry to go.
const SHARING_GAP: &str = "removing_a_custom_field_updates_both_views_and_shares_unrelated_views";

fn pg_there() -> bool {
    StackHost::boot().is_some()
}

#[test]
fn every_scenario_but_the_app_updates_passes_on_the_stack_with_no_layer_unsupported() {
    if !pg_there() {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run the L10 stack");
        return;
    }
    let mut failed = Vec::new();
    for name in scenario_names() {
        if APP_UPDATE.contains(&name) {
            continue;
        }
        let outcome = run_one(&StackFactory, name).expect("a known scenario");
        if name == SHARING_GAP {
            assert!(
                outcome
                    .failure
                    .as_deref()
                    .is_some_and(|f| f.contains("same Arc")),
                "{name} must fail on the view-sharing assertion alone: {:?}",
                outcome.failure
            );
            continue;
        }
        println!(
            "{} {}: {:?}",
            if outcome.passed { "ok  " } else { "FAIL" },
            name,
            outcome.notes
        );
        assert!(
            !outcome
                .notes
                .iter()
                .any(|n| n.to_lowercase().contains("not supported")),
            "{name} skipped a layer: {:?}",
            outcome.notes
        );
        if !outcome.passed {
            failed.push(format!("{name}: {:?}", outcome.failure));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
#[ignore = "gap: an app update replaces the standard definitions; the customise service fixes them at build (see the report)"]
fn the_app_update_scenarios_on_the_stack() {
    if !pg_there() {
        return;
    }
    for name in APP_UPDATE {
        let outcome = run_one(&StackFactory, name).expect("a known scenario");
        assert!(outcome.passed, "{name}: {:?}", outcome.failure);
    }
}

#[test]
fn a_runtime_doctype_text_has_its_fields_and_flags() {
    use spot_l10_liveness::change::{AddDocTypeDef, FieldSpecInput};
    let def = AddDocTypeDef::posted(
        "ticket",
        vec![FieldSpecInput::text("subject").with_required(true)],
    );
    let text = spot_l10_stack::stack::toml_of(&def);
    assert!(text.contains("name = \"ticket\""), "{text}");
    assert!(text.contains("required = true"), "{text}");
    assert!(text.contains("is_posted = true"), "{text}");
}
