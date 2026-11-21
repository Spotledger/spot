//! The conditions catalogue and the operations must not drift apart: every operator action a
//! condition names is exactly one operation here, or is listed with the reason it is not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use spot_mgmt_sync::actions::{not_an_operation, operation_for};
use spot_mgmt_sync::{NOT_AN_OPERATION, OPERATOR_ACTIONS, ids};
use support::*;

/// Every operator action the conditions catalogue names, with the conditions that name it.
fn named_actions() -> BTreeMap<String, Vec<String>> {
    let mut named: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in spot_sync_conditions::entries() {
        for action in &entry.def.actions {
            named
                .entry(action.clone())
                .or_default()
                .push(entry.def.id.clone());
        }
    }
    named
}

#[test]
fn every_named_action_is_one_operation_or_listed_as_not_one() {
    let catalogue = catalogue();
    let named = named_actions();
    assert!(!named.is_empty(), "the conditions catalogue names actions");
    for (action, conditions) in &named {
        let operation = operation_for(action);
        let excluded = not_an_operation(action);
        match (operation, excluded) {
            (Some(op), None) => {
                assert!(
                    catalogue.get(&ids::op_id(op).unwrap()).is_some(),
                    "{action} (named by {conditions:?}) maps to {op}, which is not registered"
                );
            }
            (None, Some(reason)) => {
                assert!(!reason.trim().is_empty(), "{action}: a reason is required");
            }
            (Some(_), Some(_)) => panic!("{action} is both an operation and not one"),
            (None, None) => panic!(
                "{action} (named by {conditions:?}) is neither an operation of spot-mgmt-sync nor in NOT_AN_OPERATION"
            ),
        }
    }
}

#[test]
fn the_tables_hold_no_stale_or_duplicate_entry() {
    let named = named_actions();
    let mut seen = BTreeSet::new();
    for (action, _) in OPERATOR_ACTIONS {
        assert!(seen.insert(action), "{action} listed twice");
        assert!(
            named.contains_key(action),
            "{action} is named by no condition"
        );
    }
    for (action, _) in NOT_AN_OPERATION {
        assert!(seen.insert(action), "{action} listed twice");
        assert!(
            named.contains_key(action),
            "{action} is named by no condition"
        );
    }
}

#[test]
fn each_mapped_action_has_its_own_operation() {
    let mut ops = BTreeSet::new();
    for (action, op) in OPERATOR_ACTIONS {
        assert!(
            ops.insert(op),
            "{action}: {op} is the target of two actions"
        );
        assert!(ids::ALL.contains(&op), "{action}: {op} is not one of ours");
    }
}

#[test]
fn the_first_command_of_a_condition_is_its_cure_and_is_runnable() {
    // Decision R1: `notify_contact` comes before `suspend_node` or `resend` wherever a
    // condition offers them together, and the operator can run all three from here.
    for entry in spot_sync_conditions::entries() {
        let actions = &entry.def.actions;
        let position = |name: &str| actions.iter().position(|a| a == name);
        if let Some(notify) = position("notify_contact") {
            for later in ["suspend_node", "resend"] {
                if let Some(other) = position(later) {
                    assert!(
                        notify < other,
                        "{}: notify_contact must come before {later}",
                        entry.def.id
                    );
                }
            }
        }
    }
    for action in ["notify_contact", "suspend_node", "resume_node", "resend"] {
        assert!(operation_for(action).is_some(), "{action}");
    }
}

#[test]
fn the_primary_remedy_of_an_entry_is_covered_when_it_is_an_operator_command() {
    for entry in spot_sync_conditions::entries() {
        if let Some(name) = &entry.primary_remedy
            && entry.def.actions.contains(name)
        {
            assert!(
                operation_for(name).is_some() || not_an_operation(name).is_some(),
                "{}: primary remedy {name} is unmapped",
                entry.def.id
            );
        }
    }
}
