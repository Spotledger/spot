//! Property tests: adding a role never removes a right, decisions do not depend on input
//! order, and a mask never shows more than its format says.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_events_contract::lifecycle::Operation;
use spot_permissions::engine::{Action, FieldLevel};
use spot_permissions::model::{AssignmentContext, MaskFormat, Principal, Role};
use spot_permissions::{DocFacts, mask};

/// DocType roles that carry no denial: adding any of them can only add rights.
const POOL: &[&str] = &[
    "Reader",
    "Writer",
    "Editor",
    "Submitter",
    "Canceler",
    "Corrector",
    "Creator",
    "Reporter",
    "Manager",
    "Viewer",
];

/// Field roles without a `deny`.
const FIELD_POOL: &[&str] = &[
    "FinRead",
    "FinFinancialOnly",
    "CreditRead",
    "FinMasked",
    "FinMix",
    "CostRead",
    "NotesRead",
    "NotesUpdate",
    "BankMasked",
    "BankReveal",
    "BankReadReveal",
    "NumberRead",
    "NumberUpdate",
];

const ACTIONS: &[Action] = &[
    Action::Read,
    Action::Correct,
    Action::Op(Operation::Create),
    Action::Op(Operation::Update),
    Action::Op(Operation::Post),
    Action::Op(Operation::Cancel),
    Action::Op(Operation::Delete),
    Action::Op(Operation::UpdatePosted),
];

fn roles_from(mask_bits: u32, pool: &[&str]) -> Vec<String> {
    pool.iter()
        .enumerate()
        .filter(|(i, _)| mask_bits & (1 << i) != 0)
        .map(|(_, r)| (*r).to_owned())
        .collect()
}

fn principal_of(roles: &[String]) -> Principal {
    let refs: Vec<&str> = roles.iter().map(String::as_str).collect();
    user("dave", &refs)
}

fn rank(level: FieldLevel) -> u8 {
    match level {
        FieldLevel::Hidden => 0,
        FieldLevel::Masked(_) => 1,
        FieldLevel::Read => 2,
        FieldLevel::ReadReveal => 3,
    }
}

proptest! {
    #[test]
    fn adding_a_role_never_removes_a_document_right(
        base in 0u32..(1 << POOL.len()),
        extra in 0u32..(1 << POOL.len()),
        state in prop_oneof![Just("draft"), Just("posted"), Just("cancelled")],
        doctype in prop_oneof![Just("project"), Just("sales_invoice"), Just("item_master")],
    ) {
        let engine = engine();
        let small = roles_from(base, POOL);
        let big = roles_from(base | extra, POOL);
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            state: Some(state.to_owned()),
            created_by: Some("dave".to_owned()),
            ..DocFacts::default()
        };
        for action in ACTIONS {
            let before = engine.can(&principal_of(&small), *action, &doc).allowed;
            let after = engine.can(&principal_of(&big), *action, &doc).allowed;
            prop_assert!(!before || after, "{action:?} on {doctype}/{state}: {small:?} -> {big:?}");
        }
    }

    #[test]
    fn adding_a_field_role_never_lowers_a_field_decision(
        base in 0u32..(1 << FIELD_POOL.len()),
        extra in 0u32..(1 << FIELD_POOL.len()),
        field in prop_oneof![Just(("sales_invoice", "total")), Just(("sales_invoice", "bank_account")), Just(("sales_invoice", "internal_notes")), Just(("sales_invoice", "number"))],
    ) {
        let engine = engine();
        let doc = DocFacts {
            doctype: field.0.to_owned(),
            state: Some("draft".to_owned()),
            ..DocFacts::default()
        };
        let facts = engine.snapshot().facts(field.0).and_then(|f| f.field(field.1).cloned()).unwrap();
        let with_doc = |bits: u32| {
            let mut roles = roles_from(bits, FIELD_POOL);
            roles.push("Writer".to_owned());
            principal_of(&roles)
        };
        let small = engine.field_access(&with_doc(base), field.0, &facts, &doc);
        let big = engine.field_access(&with_doc(base | extra), field.0, &facts, &doc);
        prop_assert!(rank(big.level) >= rank(small.level));
        prop_assert!(!small.can_update || big.can_update);
        prop_assert!(!small.can_create || big.can_create);
        prop_assert!(!small.can_report || big.can_report);
        prop_assert!(!small.can_reveal || big.can_reveal);
        prop_assert!(!small.can_reveal_bulk || big.can_reveal_bulk);
    }

    #[test]
    fn decisions_do_not_depend_on_input_order(
        bits in 0u32..(1 << POOL.len()),
        rotate in 0usize..10,
    ) {
        let engine = engine();
        let roles = roles_from(bits, POOL);
        let mut shuffled = roles.clone();
        if !shuffled.is_empty() {
            let by = rotate % shuffled.len();
            shuffled.rotate_left(by);
            shuffled.reverse();
        }
        let split = |roles: &[String]| {
            // The same roles as two assignments, in either order.
            let mid = roles.len() / 2;
            let a = AssignmentContext { id: "a".into(), roles: roles[..mid].iter().cloned().map(Role).collect(), scope: vec![] };
            let b = AssignmentContext { id: "b".into(), roles: roles[mid..].iter().cloned().map(Role).collect(), scope: vec![] };
            (a, b)
        };
        let (a1, b1) = split(&roles);
        let (a2, b2) = split(&roles);
        let p1 = Principal::user("dave", vec![a1, b1]);
        let p2 = Principal::user("dave", vec![b2, a2]);
        let doc = DocFacts {
            doctype: "sales_invoice".to_owned(),
            state: Some("draft".to_owned()),
            created_by: Some("dave".to_owned()),
            ..DocFacts::default()
        };
        for action in ACTIONS {
            prop_assert_eq!(engine.can(&p1, *action, &doc), engine.can(&p2, *action, &doc));
        }
        // And the role order inside one assignment does not matter either.
        let ordered = principal_of(&roles);
        let reversed = principal_of(&shuffled);
        for action in ACTIONS {
            prop_assert_eq!(
                engine.can(&ordered, *action, &doc).allowed,
                engine.can(&reversed, *action, &doc).allowed
            );
        }
    }

    #[test]
    fn a_mask_is_deterministic_never_longer_and_never_shows_more_than_the_format(
        value in ".{0,40}",
        n in 0u8..=255,
    ) {
        let last = MaskFormat::LastChars(n);
        let masked = mask(&value, &last);
        prop_assert_eq!(&masked, &mask(&value, &last));
        let in_chars: Vec<char> = value.chars().collect();
        let out_chars: Vec<char> = masked.chars().collect();
        prop_assert_eq!(out_chars.len(), in_chars.len());
        let shown = usize::from(n).min(8).min(in_chars.len() / 2);
        let hidden = in_chars.len() - shown;
        // Everything before the shown tail is a star; the tail is the input's own tail.
        prop_assert!(out_chars[..hidden].iter().all(|c| *c == '*'));
        prop_assert_eq!(&out_chars[hidden..], &in_chars[hidden..]);
        // Never more than half of the value is shown.
        prop_assert!(shown * 2 <= in_chars.len());

        let placeholder = mask(&value, &MaskFormat::Placeholder);
        prop_assert!(placeholder.chars().count() <= in_chars.len());
        prop_assert!(placeholder.chars().all(|c| c == '*'));
    }
}
