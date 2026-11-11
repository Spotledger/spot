//! The contract rules a message is checked against, as pure functions.
//!
//! One function, [`check_message`], serves both directions: the send handler asks it before a
//! message leaves, and the receiving gate asks it when one arrives, so the two sides can never
//! disagree about what the contract allows. Everything here reads the contract and a small
//! [`OrderState`]; nothing reads a clock, a store or a secret.

use std::collections::BTreeMap;

use serde_json::Value;
use spot_bpm_contract::{MessageDef, Outcome, ProcessContract, Transition};

use crate::error::Violation;

/// Where a conversation stands, as far as the order rules care.
#[derive(Debug, Clone, Copy)]
pub struct OrderState<'a> {
    /// The message last exchanged in either direction; `None` before the first.
    pub last: Option<&'a str>,
    /// How often each step has been taken (see [`step_key`]).
    pub counts: &'a BTreeMap<String, u32>,
    /// Whether the conversation already ended in an outcome.
    pub ended: bool,
}

/// The key a transition is counted under: `after>by>next`.
#[must_use]
pub fn step_key(transition: &Transition) -> String {
    format!(
        "{}>{}>{}",
        transition.after.as_deref().unwrap_or(""),
        transition.by,
        transition
            .next_message()
            .or_else(|| transition.next_outcome())
            .unwrap_or("")
    )
}

/// The transition that lets `by` send `name` after `last`, and whether it still has a use left.
fn find_step<'c>(
    contract: &'c ProcessContract,
    last: Option<&str>,
    by: &str,
    name: &str,
    counts: &BTreeMap<String, u32>,
) -> Result<&'c Transition, Violation> {
    let mut seen = false;
    for transition in &contract.order {
        if transition.after.as_deref() != last
            || transition.by != by
            || transition.next_message() != Some(name)
        {
            continue;
        }
        seen = true;
        let used = counts.get(&step_key(transition)).copied().unwrap_or(0);
        if used < transition.bound.unwrap_or(1) {
            return Ok(transition);
        }
    }
    Err(if seen {
        Violation::StepExhausted
    } else {
        Violation::OutOfOrder
    })
}

/// Checks that `from_role` may send `name` to `to_role` now, with this payload.
///
/// The order is: the conversation is still open, the message is defined, it travels between
/// these roles in this direction, the contract allows it after the previous message, the schema
/// version is the contract's, and the payload fits the field list. The first broken rule is
/// reported.
///
/// # Errors
/// The [`Violation`] of the first broken rule.
pub fn check_message<'c>(
    contract: &'c ProcessContract,
    from_role: &str,
    to_role: &str,
    name: &str,
    schema_version: u32,
    payload: &Value,
    state: &OrderState<'_>,
) -> Result<&'c MessageDef, Violation> {
    if state.ended {
        return Err(Violation::ConversationEnded);
    }
    let def = contract.message(name).ok_or(Violation::MessageUnknown)?;
    if def.from != from_role || def.to != to_role {
        return Err(Violation::WrongDirection);
    }
    find_step(contract, state.last, from_role, name, state.counts)?;
    if def.schema_version != schema_version {
        return Err(Violation::SchemaUnsupported);
    }
    let errors = def.schema().validate(payload);
    if !errors.is_empty() {
        let mut fields = Vec::new();
        let mut unknown = 0;
        for error in &errors {
            if def.field(&error.field).is_some() {
                fields.push(error.field.clone());
            } else {
                unknown += 1;
            }
        }
        fields.sort();
        fields.dedup();
        return Err(Violation::PayloadInvalid { fields, unknown });
    }
    Ok(def)
}

/// Whether `by` may open a conversation with `name`.
///
/// # Errors
/// [`Violation::NotEntry`] when the contract has no starting step for it.
pub fn check_entry(contract: &ProcessContract, by: &str, name: &str) -> Result<(), Violation> {
    find_step(contract, None, by, name, &BTreeMap::new())
        .map(|_| ())
        .map_err(|_| Violation::NotEntry)
}

/// Counts the step `by` takes when sending `name` after `last`.
pub fn take_step(
    contract: &ProcessContract,
    last: Option<&str>,
    by: &str,
    name: &str,
    counts: &mut BTreeMap<String, u32>,
) {
    if let Ok(transition) = find_step(contract, last, by, name, counts) {
        let used = counts.entry(step_key(transition)).or_insert(0);
        *used = used.saturating_add(1);
    }
}

/// The outcome a conversation ends in once `message` has been exchanged, when nothing more may
/// follow it.
///
/// A message ends the conversation when the contract lets no further message follow it and
/// declares at least one outcome after it. Of several outcomes, one a wait limit names (the
/// "nobody answered" outcome) is not the natural ending; the first of the others is.
#[must_use]
pub fn ending_after<'c>(contract: &'c ProcessContract, message: &str) -> Option<&'c Outcome> {
    let after = contract.outgoing(message);
    if after.iter().any(|t| t.next_message().is_some()) {
        return None;
    }
    let outcomes: Vec<&Outcome> = after
        .iter()
        .filter_map(|t| t.next_outcome())
        .filter_map(|name| contract.outcome(name))
        .collect();
    let timeout_outcomes: Vec<&str> = contract
        .timeouts
        .iter()
        .filter(|t| t.message == message)
        .filter_map(|t| t.outcome.as_deref())
        .collect();
    outcomes
        .iter()
        .find(|o| !timeout_outcomes.contains(&o.name.as_str()))
        .or_else(|| outcomes.first())
        .copied()
}

/// The outcome the contract gives a wait of `role` for `message` that ran out of time.
#[must_use]
pub fn timeout_outcome<'c>(
    contract: &'c ProcessContract,
    message: &str,
    role: &str,
) -> Option<&'c Outcome> {
    let name = contract.timeout(message, role)?.outcome.as_deref()?;
    contract.outcome(name)
}
