//! The grant-time check: only conflicts **created** by the proposed assignment are blamed;
//! a conflict that already existed is not, though a grant into a new scope of an
//! already-violated rule is new.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_sod::{GrantCheck, analyze, check_grant};

#[test]
fn a_clean_grant_is_clear() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "jane",
        "Maintain supplier master",
        Some("Company A"),
    )];
    let proposed = common::duty("jane", "Create purchase order", Some("Company A"));
    let check = check_grant(&rules, &catalogue, &existing, &proposed, &[]);
    assert_eq!(check, GrantCheck::Clear);
}

#[test]
fn a_conflicting_grant_is_reported_with_its_holds() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "jane",
        "Maintain supplier master",
        Some("Company A"),
    )];
    let proposed = common::duty("jane", "Approve payment run", Some("Company A"));
    let GrantCheck::Conflicts(conflicts) =
        check_grant(&rules, &catalogue, &existing, &proposed, &[])
    else {
        panic!("the grant creates a conflict");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rule_id, "sod.vendor-master.payments");
    assert_eq!(conflicts[0].user, "jane");
    assert_eq!(conflicts[0].scope.as_deref(), Some("Company A"));
    assert_eq!(conflicts[0].held.len(), 2, "both sides named");
    // The check equals the set difference of the two analyses (the property test proves it
    // generally; this pins a concrete case).
    let mut with = existing.clone();
    with.push(proposed.clone());
    let expected = analyze(&rules, &catalogue, &with, &[])
        .into_iter()
        .filter(|c| !analyze(&rules, &catalogue, &existing, &[]).contains(c))
        .collect::<Vec<_>>();
    assert_eq!(conflicts, expected);
}

#[test]
fn a_conflict_that_already_existed_is_not_blamed_on_the_grant() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // jane is ALREADY over the line before the proposed grant arrives.
    let existing = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
    ];
    assert!(
        !analyze(&rules, &catalogue, &existing, &[]).is_empty(),
        "the fixture really does start conflicted"
    );
    // Granting a third, harmless duty must not re-blame the old conflict.
    let proposed = common::duty("jane", "Create purchase order", Some("Company A"));
    assert_eq!(
        check_grant(&rules, &catalogue, &existing, &proposed, &[]),
        GrantCheck::Clear
    );
    // Granting a duplicate of an existing duty changes nothing either.
    let duplicate = common::duty("jane", "Maintain supplier master", Some("Company A"));
    assert_eq!(
        check_grant(&rules, &catalogue, &existing, &duplicate, &[]),
        GrantCheck::Clear
    );
    // And granting the second side again (same scope) is still not a new conflict.
    let second_again = common::duty("jane", "Approve payment run", Some("Company A"));
    assert_eq!(
        check_grant(&rules, &catalogue, &existing, &second_again, &[]),
        GrantCheck::Clear
    );
}

#[test]
fn a_grant_into_a_new_scope_of_a_violated_rule_is_new() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // jane holds both conflicting duties in Company A; the grant adds the master duty in
    // Company B, which pairs with nothing yet — clear. Then the payment duty in Company B
    // would create a **second** conflict (Company B), which is new and is blamed.
    let existing = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
    ];
    let masters_b = common::duty("jane", "Maintain supplier master", Some("Company B"));
    assert_eq!(
        check_grant(&rules, &catalogue, &existing, &masters_b, &[]),
        GrantCheck::Clear
    );
    let mut now_with_b = existing.clone();
    now_with_b.push(masters_b);
    let payments_b = common::duty("jane", "Approve payment run", Some("Company B"));
    let GrantCheck::Conflicts(conflicts) =
        check_grant(&rules, &catalogue, &now_with_b, &payments_b, &[])
    else {
        panic!("the Company B pair is a new conflict");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].scope.as_deref(), Some("Company B"));
}

#[test]
fn a_grant_of_the_second_side_through_a_role_is_blamed_the_same() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "jane",
        "Maintain supplier master",
        Some("Company A"),
    )];
    // The duty reaches jane through the Finance Approver role: same conflict, named with its
    // provenance.
    let proposed = common::role("jane", "Finance Approver", Some("Company A"));
    let GrantCheck::Conflicts(conflicts) =
        check_grant(&rules, &catalogue, &existing, &proposed, &[])
    else {
        panic!("granting the role creates the conflict");
    };
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].held.iter().any(|h| h.side == 1));
    assert!(
        conflicts[0]
            .held
            .iter()
            .any(|h| matches!(&h.via, spot_sod::Via::ThroughRole { role } if role.as_str() == "Finance Approver"))
    );
}

#[test]
fn an_any_scope_grant_conflicts_whatever_the_scopes() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "sam",
        "Maintain bank details",
        Some("Company A"),
    )];
    let proposed = common::duty("sam", "Release payment", Some("Company B"));
    let GrantCheck::Conflicts(conflicts) =
        check_grant(&rules, &catalogue, &existing, &proposed, &[])
    else {
        panic!("any_scope rules cross scopes");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rule_id, "sod.bank-details.release");
}
