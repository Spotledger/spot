//! Grants carry the id the injected source returned, and a plan names an assignment by it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{ScriptedIds, T0, counting_ids, later, rule_set, settle, users};
use spot_iam_assign::{Assignment, Change, Facts, IdSource, Plan, Source, plan_rules_guarded};

/// The id of a grant is the source's next id, not something derived from the reason and user.
#[test]
fn a_grant_carries_the_id_the_source_returned() {
    let set = rule_set();
    let mut ids = ScriptedIds::new(&["0190f0aa-0000-7000-8000-000000000001"]);
    let all = vec![(
        "ben".to_owned(),
        Facts::new()
            .text("department", "Finance")
            .text("position", "Accountant")
            .text("company", "C1"),
    )];
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &[],
        &all,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let granted: Vec<&Assignment> = plan.grants().collect();
    assert_eq!(granted.len(), 1);
    assert_eq!(
        granted[0].id, "0190f0aa-0000-7000-8000-000000000001",
        "the derived `rule:.../<user>` id must be gone"
    );
}

/// One plan may grant one assignment and end another; each change names its assignment by the
/// id that assignment has: the source's for the grant, the stored one for the end.
#[test]
fn one_plan_names_each_assignment_by_its_own_id() {
    let set = rule_set();
    let mut ids = ScriptedIds::new(&["0190f0aa-0000-7000-8000-000000000002"]);
    // Ana keeps her sales assignment; Ben's accountant assignment exists but his condition fails,
    // so the same plan ends Ben's and grants Ana's audit assignment.
    let mut ana = Assignment::new("stored-ana", "ana", "Sales User");
    ana.source = Source::Rule;
    ana.reason = Some("rule:sales".into());
    let mut ben = Assignment::new("stored-ben", "ben", "Accountant");
    ben.source = Source::Rule;
    ben.reason = Some("rule:accountants".into());
    let current = vec![ana, ben];
    let all = vec![
        (
            "ana".to_owned(),
            Facts::new()
                .text("department", "Sales")
                .texts("projects", &["audit"]),
        ),
        (
            "ben".to_owned(),
            Facts::new()
                .text("department", "Legal")
                .texts("projects", &[]),
        ),
    ];
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &current,
        &all,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let granted = plan.grants().next().unwrap();
    assert_eq!(granted.id, "0190f0aa-0000-7000-8000-000000000002");
    assert_eq!(granted.role.as_str(), "Auditor");
    let ended: Vec<&str> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::End { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ended, ["stored-ben"]);
    // Applying the plan touches exactly those two assignments.
    let after = plan.apply(&current);
    assert!(after.iter().any(|a| a.id == "stored-ana"));
    assert_eq!(after.len(), 3);
}

/// A grant and an end for the same assignment in one plan refer to each other by the id the
/// source returned, so applying the plan ends the grant it made.
#[test]
fn a_grant_and_an_end_in_one_plan_share_the_sources_id() {
    let mut ids = ScriptedIds::new(&["0190f0aa-0000-7000-8000-000000000003"]);
    let id = ids.next();
    let mut granted = Assignment::new(&id, "ben", "Accountant");
    granted.source = Source::Rule;
    granted.reason = Some("rule:accountants".into());
    let plan = Plan {
        changes: vec![
            Change::Grant(granted),
            Change::End {
                id: id.clone(),
                user: "ben".to_owned(),
                role: "Accountant".into(),
                at: later(1),
                reason: "the condition no longer holds".to_owned(),
            },
        ],
        events: Vec::new(),
        errors: Vec::new(),
        ended_marker: Vec::new(),
    };
    assert_eq!(plan.changes[0].id(), id);
    assert_eq!(plan.changes[1].id(), id);
    let after = plan.apply(&[]);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, id);
    assert_eq!(after[0].valid_to, Some(later(1)));
}

/// A grant from one plan is ended, and a replaced one keeps its id, in later plans.
#[test]
fn a_later_plan_ends_or_replaces_the_grant_by_its_id() {
    let set = rule_set();
    let mut ids = counting_ids();
    let all = users();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let ben = state.iter().find(|a| a.user == "ben").unwrap();
    let granted_id = ben.id.clone();
    assert!(
        granted_id.starts_with("id-"),
        "the id is the source's, not the derived reason/user string: {granted_id}"
    );
    assert_ne!(granted_id, "rule:accountants/ben");

    // A changed scope replaces the assignment in place: same id, from the source's grant.
    let mut moved = all.clone();
    moved[1].1 = Facts::new()
        .text("department", "Finance")
        .text("position", "Accountant")
        .text("company", "C9");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &moved,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    let Change::Replace { id, with } = &plan.changes[0] else {
        panic!("expected a replace")
    };
    assert_eq!(id, &granted_id);
    assert_eq!(with.id, granted_id);
    let state = settle(&plan, &state);

    // The condition no longer holds: the end names the very id the source handed out.
    moved[1].1 = Facts::new().text("department", "Legal");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &moved,
        later(2),
        &spot_iam_assign::OutsideGuard::none(),
    );
    let Change::End { id, .. } = &plan.changes[0] else {
        panic!("expected an end")
    };
    assert_eq!(id, &granted_id);
    assert_eq!(plan.ended_marker, [granted_id]);
}
