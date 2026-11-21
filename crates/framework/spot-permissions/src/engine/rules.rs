//! Free helper rules the decision methods share: the state refusals, scope blocking and
//! the deterministic reason order (`use`d from every module that decides).
//!
//! They are shared, not private, because `field_access`, `check_links` and `can` all read
//! them and one definition keeps the three answers consistent.

use spot_app_api::prelude::Value;
use spot_events_contract::lifecycle::Operation;

use crate::engine::{Action, DocFacts};
use crate::model::{
    AssignmentContext, FieldCondition, FieldTarget, Principal, Right, Role, ShareRight,
    UserPermission,
};
use crate::reason::Reason;
use crate::snapshot::DoctypeFacts;

/// The state rule that refuses `action` outright, when one does (ADR-42, ADR-51).
pub(super) fn state_refusal(action: Action, doc: &DocFacts) -> Option<Reason> {
    let state = doc.state.as_deref()?;
    let refused = |s: &str, a: &str| {
        Some(Reason::PostedState {
            state: s.to_owned(),
            refused: a.to_owned(),
        })
    };
    match (state, action) {
        ("posted", Action::Op(Operation::Update)) => refused(state, "update"),
        ("cancelled", Action::Op(op)) if !matches!(op, Operation::Rename | Operation::Delete) => {
            refused(state, action.as_str())
        }
        (other, Action::Op(Operation::UpdatePosted)) if other != "posted" => {
            refused(other, "update_posted")
        }
        (other, Action::Op(Operation::Cancel)) if other != "posted" => refused(other, "cancel"),
        (other, Action::Op(Operation::Post)) if other != "draft" => refused(other, "post"),
        (other, Action::Correct) if other != "posted" => refused(other, "correct"),
        _ => None,
    }
}

/// The `(field path, value)` pairs one assignment's scope blocks on this document.
pub(super) fn scope_blocks(
    assignment: &AssignmentContext,
    doc: &DocFacts,
    facts: &DoctypeFacts,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for up in sorted_scope(assignment) {
        if up.doctype.as_deref().is_some_and(|d| d != doc.doctype) {
            continue;
        }
        for lf in &facts.links {
            let Some(value) = doc.values.get(&lf.name) else {
                continue;
            };
            let target = lf.target.clone().unwrap_or_default();
            let path = format!("{}.{}", doc.doctype, lf.name);
            if up.restricts(&doc.doctype, &path, &target) {
                let text = value_text(value);
                if !up.allowed.iter().any(|a| a == &text) {
                    out.push((path, text));
                }
            }
        }
    }
    out
}

pub(super) fn sorted_scope(a: &AssignmentContext) -> Vec<&UserPermission> {
    let mut ups: Vec<&UserPermission> = a.scope.iter().collect();
    ups.sort();
    ups
}

pub(super) fn sorted_roles(a: &AssignmentContext) -> Vec<&Role> {
    let mut roles: Vec<&Role> = a.roles.iter().collect();
    roles.sort();
    roles.dedup();
    roles
}

pub(super) fn target_text(target: &FieldTarget) -> String {
    match target {
        FieldTarget::Field(path) => format!("field:{path}"),
        FieldTarget::Group(name) => format!("group:{}", name.0),
    }
}

/// Whether a grant's condition holds on this document for this principal.
pub(super) fn condition_holds(
    condition: Option<&FieldCondition>,
    principal: &Principal,
    doc: &DocFacts,
) -> bool {
    match condition {
        None => true,
        Some(FieldCondition::OwnRecord) => doc.owned_by(principal),
        Some(FieldCondition::StatusIn(states)) => doc
            .state
            .as_ref()
            .is_some_and(|state| states.iter().any(|s| s == state)),
    }
}

/// Does the share right carry the asked `right`?
pub(super) fn share_grants(share: ShareRight, right: Right) -> bool {
    matches!(
        (share, right),
        (ShareRight::Read, Right::Read)
            | (ShareRight::Write, Right::Write)
            | (ShareRight::Submit, Right::Submit)
            | (ShareRight::Cancel, Right::Cancel)
            | (ShareRight::Correct, Right::Correct)
            | (ShareRight::Share, Right::Share)
    )
}

/// The deterministic reason order: the enum's derived order, so the same decision is stable
/// whatever order the grants were loaded in.
pub(super) fn sorted(mut reasons: Vec<Reason>) -> Vec<Reason> {
    reasons.sort();
    reasons
}

pub(super) fn dedup(mut reasons: Vec<Reason>) -> Vec<Reason> {
    reasons.sort();
    reasons.dedup();
    reasons
}

/// The text form of a value, as value restrictions compare them: a record id by its canonical
/// text, text values as themselves.
pub(super) fn value_text(v: &Value) -> String {
    match v {
        Value::Id(id) => id.to_string(),
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}
