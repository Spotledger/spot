//! What changed between two versions of a module's methods, for the migration plan (WP-MIG).
//!
//! Installing, upgrading and uninstalling an app swaps its TypeScript controllers through the
//! registry's immutable [`ControllerSet`](spot_controllers::ControllerSet), so an in-flight
//! operation keeps the set it began with. An upgrade that changes a method's arguments in a way
//! existing callers cannot survive is a **breaking** change; [`diff_method_decls`] reports every
//! method that changed, and whether it breaks callers.

use crate::contract::{Contract, MethodSpec};

/// How a method changed between two module versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodChangeKind {
    /// The method is new: callers that used it before did not.
    Added,
    /// The method is gone: every existing caller breaks.
    Removed,
    /// The declaration changed in a way an existing caller may not survive.
    Breaking,
    /// The declaration changed but every existing caller still works.
    Compatible,
}

/// One method that changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodChange {
    /// The method's name.
    pub method: String,
    /// How it changed.
    pub kind: MethodChangeKind,
    /// A developer-facing sentence naming the change.
    pub detail: String,
}

/// Whether `kind` is a change a migration must plan for.
impl MethodChange {
    /// Whether this change breaks existing callers.
    pub fn is_breaking(&self) -> bool {
        matches!(
            self.kind,
            MethodChangeKind::Breaking | MethodChangeKind::Removed
        )
    }
}

/// Every method that differs between `old` and `new`, in name order.
pub fn diff_method_decls(old: &Contract, new: &Contract) -> Vec<MethodChange> {
    let mut changes = Vec::new();
    for (name, before) in &old.methods {
        match new.methods.get(name) {
            None => changes.push(MethodChange {
                method: name.clone(),
                kind: MethodChangeKind::Removed,
                detail: "the method is no longer declared".to_owned(),
            }),
            Some(after) => {
                if let Some(change) = compare(name, before, after) {
                    changes.push(change);
                }
            }
        }
    }
    for name in new.methods.keys() {
        if !old.methods.contains_key(name) {
            changes.push(MethodChange {
                method: name.clone(),
                kind: MethodChangeKind::Added,
                detail: "a new method".to_owned(),
            });
        }
    }
    changes.sort_by(|a, b| a.method.cmp(&b.method));
    changes
}

fn compare(name: &str, before: &MethodSpec, after: &MethodSpec) -> Option<MethodChange> {
    let mut breaking = Vec::new();
    let mut compatible = Vec::new();

    for (arg, tag) in &before.args.properties {
        match after.args.properties.get(arg) {
            None => breaking.push(format!("argument `{arg}` was removed")),
            Some(other) if other != tag => breaking.push(format!("argument `{arg}` changed type")),
            Some(_) => {}
        }
    }
    for (arg, tag) in &after.args.properties {
        if !before.args.properties.contains_key(arg) {
            if tag.optional {
                compatible.push(format!("optional argument `{arg}` was added"));
            } else {
                breaking.push(format!("required argument `{arg}` was added"));
            }
        }
    }
    for arg in &after.args.required {
        if !before.args.required.contains(arg) && before.args.properties.contains_key(arg) {
            breaking.push(format!("argument `{arg}` became required"));
        }
    }
    for arg in &before.args.required {
        if !after.args.required.contains(arg) && after.args.properties.contains_key(arg) {
            compatible.push(format!("argument `{arg}` became optional"));
        }
    }
    if before.args.open != after.args.open {
        if before.args.open && !after.args.open {
            breaking.push("unknown arguments are no longer allowed".to_owned());
        } else {
            compatible.push("unknown arguments are now allowed".to_owned());
        }
    }
    if before.result != after.result {
        breaking.push("the result schema changed".to_owned());
    }
    if before.mutates != after.mutates {
        breaking.push("whether the method changes documents changed".to_owned());
    }
    if before.right != after.right {
        compatible.push(format!("the right changed to `{}`", after.right));
    }

    if !breaking.is_empty() {
        Some(MethodChange {
            method: name.to_owned(),
            kind: MethodChangeKind::Breaking,
            detail: breaking.join("; "),
        })
    } else if !compatible.is_empty() {
        Some(MethodChange {
            method: name.to_owned(),
            kind: MethodChangeKind::Compatible,
            detail: compatible.join("; "),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::contract::parse_contract;
    use serde_json::json;

    fn contract(method: serde_json::Value) -> Contract {
        parse_contract(&json!({ "doctype": "invoice", "methods": { "approve": method } })).unwrap()
    }

    #[test]
    fn a_removed_method_breaks() {
        let old = contract(json!({"right": "write"}));
        let new = parse_contract(&json!({"doctype": "invoice", "methods": {}})).unwrap();
        let changes = diff_method_decls(&old, &new);
        assert_eq!(changes[0].kind, MethodChangeKind::Removed);
        assert!(changes[0].is_breaking());
    }

    #[test]
    fn a_new_required_argument_breaks_but_an_optional_one_does_not() {
        let old = contract(json!({"right": "write", "args": {"amount": "decimal"}}));
        let breaking =
            contract(json!({"right": "write", "args": {"amount": "decimal", "reason": "text"}}));
        let changes = diff_method_decls(&old, &breaking);
        assert_eq!(changes[0].kind, MethodChangeKind::Breaking);

        let compatible =
            contract(json!({"right": "write", "args": {"amount": "decimal", "reason": "text?"}}));
        let changes = diff_method_decls(&old, &compatible);
        assert_eq!(changes[0].kind, MethodChangeKind::Compatible);
    }

    #[test]
    fn an_unchanged_method_reports_nothing() {
        let old = contract(json!({"right": "write", "args": {"amount": "decimal"}}));
        assert!(diff_method_decls(&old, &old.clone()).is_empty());
    }
}
