//! Reference-duty hygiene (the Workday finding: a delivered role can carry a conflict inside
//! itself) and the TOML load errors for every `sod.*` code.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_sod::{Duty, Role, RoleCatalogue, RuleSet, internal_conflicts};

#[test]
fn internal_conflicts_find_the_role_holding_both_sides() {
    let rules = common::rules();
    let finding = common::finding_catalogue();
    let found = internal_conflicts(&rules, &finding);
    assert_eq!(found.len(), 1, "{found:#?}");
    let c = &found[0];
    assert_eq!(c.rule_id, "sod.vendor-master.payments");
    assert!(c.role.as_str() == "Super Clerk");
    assert!(
        c.first_side
            .contains(&spot_sod::Duty::new("Maintain supplier master").expect("duty"))
    );
    assert!(
        c.second_side
            .contains(&spot_sod::Duty::new("Approve payment run").expect("duty"))
    );
}

#[test]
fn a_clean_reference_catalogue_has_no_internal_conflicts() {
    let rules = common::rules();
    let clean = common::clean_catalogue();
    assert!(
        internal_conflicts(&rules, &clean).is_empty(),
        "the delivered reference roles are authored without internal conflicts"
    );
}

#[test]
fn every_sided_role_pair_is_found_and_results_are_sorted() {
    let rules = common::rules();
    let catalogue = RoleCatalogue::from_toml(
        r#"
[[roles]]
name = "B Role"
duties = ["Maintain supplier master", "Approve payment run"]
[[roles]]
name = "A Role"
duties = ["Maintain bank details", "Release payment"]
"#,
    )
    .expect("loads");
    let found = internal_conflicts(&rules, &catalogue);
    assert_eq!(found.len(), 2);
    // Sorted by (rule id, role): bank-details before vendor-master; A Role before B Role
    // within one rule — here the two rules differ, so rule id decides.
    assert_eq!(found[0].rule_id, "sod.bank-details.release");
    assert_eq!(found[1].rule_id, "sod.vendor-master.payments");
}

#[test]
fn load_errors_for_each_sod_code() {
    // Duplicate rule id: sod.duplicate_id, naming the id.
    let dup = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.twice"
title = "First"
description = "The first rule with this id."
severity = "low"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
duties = ["B"]
[[rules]]
id = "sod.twice"
title = "Second"
description = "The second rule with the same id."
severity = "low"
[[rules.sides]]
duties = ["C"]
[[rules.sides]]
duties = ["D"]
"#,
    )
    .expect_err("the duplicate id is an error");
    assert_eq!(dup.code(), "sod.duplicate_id", "{dup:?}");
    assert_eq!(dup.rule(), "sod.twice");

    // Fewer than two sides: sod.side_count.
    let one_side = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.one-side"
title = "One side"
description = "A rule with a single side."
severity = "low"
[[rules.sides]]
duties = ["A"]
"#,
    )
    .expect_err("one side is an error");
    assert_eq!(one_side.code(), "sod.side_count", "{one_side:?}");
    assert_eq!(one_side.rule(), "sod.one-side");

    // An empty side: sod.side_empty.
    let empty_side = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.empty-side"
title = "Empty side"
description = "A rule with an empty side."
severity = "low"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
roles = ["R"]
"#,
    )
    .expect_err("an empty side is an error");
    assert_eq!(empty_side.code(), "sod.side_empty", "{empty_side:?}");
    assert_eq!(empty_side.rule(), "sod.empty-side");

    // The same duty on two sides: sod.side_overlap.
    let overlap = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.overlap"
title = "Overlapping"
description = "The same duty sits on both sides."
severity = "low"
[[rules.sides]]
duties = ["A", "B"]
[[rules.sides]]
duties = ["B", "C"]
"#,
    )
    .expect_err("a shared duty is an error");
    assert_eq!(overlap.code(), "sod.side_overlap", "{overlap:?}");
    assert_eq!(overlap.rule(), "sod.overlap");
    assert!(
        overlap.to_string().contains("B"),
        "the duty is named: {overlap}"
    );

    // Unknown severity: sod.severity_unknown.
    let severity = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.bad-severity"
title = "Bad severity"
description = "The severity is not one the model knows."
severity = "catastrophic"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
duties = ["B"]
"#,
    )
    .expect_err("an unknown severity is an error");
    assert_eq!(severity.code(), "sod.severity_unknown", "{severity:?}");
    assert_eq!(severity.rule(), "sod.bad-severity");

    // Blank rule id: sod.name_empty.
    let blank = RuleSet::from_toml(
        r#"
[[rules]]
id = "   "
title = "No id"
description = "The id is blank."
severity = "low"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
duties = ["B"]
"#,
    )
    .expect_err("a blank id is an error");
    assert_eq!(blank.code(), "sod.name_empty", "{blank:?}");

    // Bad TOML and an unknown key: sod.parse_failed; the unknown key too (deny_unknown_fields).
    let bad_toml = RuleSet::from_toml("this is = not = toml ][").expect_err("bad TOML is an error");
    assert_eq!(bad_toml.code(), "sod.parse_failed");
    let unknown_key = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.typo"
title = "Typo"
description = "The severity key is misspelled."
serverity = "low"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
duties = ["B"]
"#,
    )
    .expect_err("an unknown key is an error");
    assert_eq!(unknown_key.code(), "sod.parse_failed", "{unknown_key:?}");
    // An unknown scope mode is a parse error naming the rule id.
    let bad_scope = RuleSet::from_toml(
        r#"
[[rules]]
id = "sod.bad-scope"
title = "Bad scope"
description = "The scope mode is misspelled."
severity = "low"
scope = "some_scope"
[[rules.sides]]
duties = ["A"]
[[rules.sides]]
duties = ["B"]
"#,
    )
    .expect_err("an unknown scope mode is an error");
    assert_eq!(bad_scope.code(), "sod.parse_failed", "{bad_scope:?}");
    assert_eq!(bad_scope.rule(), "sod.bad-scope");
}

#[test]
fn catalogue_load_errors_carry_codes() {
    let blank_name = RoleCatalogue::from_toml("[[roles]]\nname = \"\"\n")
        .expect_err("a blank role name is an error");
    assert_eq!(blank_name.code(), "sod.name_empty");
    let duplicate = RoleCatalogue::from_toml(
        "[[roles]]\nname = \"R\"\nduties = [\"A\"]\n[[roles]]\nname = \"R\"\nduties = [\"B\"]\n",
    )
    .expect_err("a duplicate role is an error");
    assert_eq!(duplicate.code(), "sod.duplicate_id");
    let bad = RoleCatalogue::from_toml("nonsense ][").expect_err("bad TOML is an error");
    assert_eq!(bad.code(), "sod.parse_failed");
    let empty_duty = RoleCatalogue::from_toml("[[roles]]\nname = \"R\"\nduties = [\"  \"]\n")
        .expect_err("a blank duty is an error");
    assert_eq!(empty_duty.code(), "sod.name_empty");
}

#[test]
fn empty_rule_text_is_an_empty_set_and_a_round_trip_holds() {
    let empty = RuleSet::from_toml("").expect("empty text is an empty set");
    assert!(empty.as_slice().is_empty());
    // The seeded rules survive to_string -> from_toml with the same rules.
    let seeded = common::rules();
    let text = toml::to_string(&seeded).expect("rules serialise");
    let back = RuleSet::from_toml(&text).expect("re-load");
    assert_eq!(back, seeded);
    // A rule serialises with snake_case severity and scope; sides carry duties and roles.
    let text = toml::to_string(&seeded).expect("serialise");
    assert!(text.contains("severity = \"critical\""), "{text}");
    assert!(text.contains("scope = \"same_scope\"") || text.contains("scope = \"any_scope\""));
}

#[test]
fn assignments_load_from_toml_with_duty_or_role() {
    let text = r#"
assignments = [
    { user = "jane", duty = "Maintain supplier master", scope = "Company A" },
    { user = "rob", role = "Finance Approver" },
]
"#;
    #[derive(serde::Deserialize)]
    struct File {
        assignments: Vec<spot_sod::Assignment>,
    }
    let file: File = toml::from_str(text).expect("assignments load");
    assert_eq!(file.assignments.len(), 2);
    assert!(
        matches!(&file.assignments[0].grant, spot_sod::Grant::Duty(d) if d.as_str() == "Maintain supplier master")
    );
    assert!(
        matches!(&file.assignments[1].grant, spot_sod::Grant::Role(r) if r.as_str() == "Finance Approver")
    );
    // Both keys is an error, so is neither; a blank user is one too.
    for bad in [
        "{ user = \"x\", duty = \"D\", role = \"R\" }",
        "{ user = \"x\" }",
        "{ user = \" \", duty = \"D\" }",
    ] {
        let text = format!("assignments = [{bad}]");
        let result: Result<File, _> = toml::from_str(&text);
        let error = result.err().expect("the bad assignment is refused");
        let message = error.to_string();
        assert!(
            message.contains("duty") || message.contains("user"),
            "the error names the problem: {message}"
        );
    }
}

#[test]
fn newtype_validation_trims_and_refuses_blanks() {
    let duty = Duty::new("  Maintain suppliers  ").expect("trimmed duty");
    assert_eq!(duty.as_str(), "Maintain suppliers");
    assert!(Duty::new("   ").is_none());
    let role = Role::new("\tAccounts Payable\n").expect("trimmed role");
    assert_eq!(role.as_str(), "Accounts Payable");
    assert!(Role::new("").is_none());
    // Assignment constructors trim too.
    let a = spot_sod::Assignment::duty("jane", "  Maintain suppliers  ", None);
    assert!(matches!(&a.grant, spot_sod::Grant::Duty(d) if d.as_str() == "Maintain suppliers"));
}
