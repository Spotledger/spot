//! [`KeyScope`]: what an API key may do, as data, and the proof that it is a subset of what
//! its owner may do (ADR-070 section 5, rule 3).
//!
//! A key never holds a right of its own. Its decision is the owner's decision **at the time of
//! the request**, narrowed by the key's allow-list and its value restriction. A key that was
//! created within the owner's rights therefore shrinks when the owner loses a role, without
//! being edited, and can never exceed the owner.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use spot_permissions::{
    Action, Decision, DocFacts, PermissionEngine, Principal, Reason, UserPermission,
};
use spot_storage_contract::Value;

use crate::error::{KEY_OWNER_INACTIVE, KEY_OWNER_TYPE, SCOPE_EMPTY, SCOPE_EXCESS};

/// The doctype that stands for "every DocType the snapshot knows".
pub const ANY_DOCTYPE: &str = "*";

/// An action a key may be allowed, serialised in the permission engine's text form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Read a document.
    Read,
    /// Create a document.
    Create,
    /// Update a draft document.
    Update,
    /// Post a draft.
    Post,
    /// Cancel a posted document.
    Cancel,
    /// Delete a document.
    Delete,
    /// Rename a document.
    Rename,
    /// Update the `allow_on_posted` fields of a posted document.
    UpdatePosted,
    /// Cancel and amend in one transaction.
    Correct,
}

impl KeyAction {
    /// Every action, in a fixed order.
    #[must_use]
    pub fn all() -> [KeyAction; 9] {
        [
            KeyAction::Read,
            KeyAction::Create,
            KeyAction::Update,
            KeyAction::Post,
            KeyAction::Cancel,
            KeyAction::Delete,
            KeyAction::Rename,
            KeyAction::UpdatePosted,
            KeyAction::Correct,
        ]
    }

    /// The permission engine's action.
    #[must_use]
    pub fn to_action(self) -> Action {
        // The text forms are the engine's own; `all()` covers every variant, and a test
        // proves the round trip.
        Action::parse(self.as_str()).unwrap_or(Action::Read)
    }

    /// The key action for an engine action.
    #[must_use]
    pub fn from_action(action: Action) -> KeyAction {
        match action.as_str() {
            "create" => KeyAction::Create,
            "update" => KeyAction::Update,
            "post" => KeyAction::Post,
            "cancel" => KeyAction::Cancel,
            "delete" => KeyAction::Delete,
            "rename" => KeyAction::Rename,
            "update_posted" => KeyAction::UpdatePosted,
            "correct" => KeyAction::Correct,
            _ => KeyAction::Read,
        }
    }

    /// The stable text form (the engine's).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            KeyAction::Read => "read",
            KeyAction::Create => "create",
            KeyAction::Update => "update",
            KeyAction::Post => "post",
            KeyAction::Cancel => "cancel",
            KeyAction::Delete => "delete",
            KeyAction::Rename => "rename",
            KeyAction::UpdatePosted => "update_posted",
            KeyAction::Correct => "correct",
        }
    }
}

/// One allow-list row: a DocType (or `"*"`) and the actions allowed on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AllowRule {
    /// The DocType by storage name, or `"*"` for every DocType.
    pub doctype: String,
    /// The actions allowed on it.
    pub actions: BTreeSet<KeyAction>,
}

impl AllowRule {
    /// A rule for one DocType.
    #[must_use]
    pub fn new(doctype: &str, actions: impl IntoIterator<Item = KeyAction>) -> AllowRule {
        AllowRule {
            doctype: doctype.to_owned(),
            actions: actions.into_iter().collect(),
        }
    }

    fn covers(&self, doctype: &str, action: KeyAction) -> bool {
        (self.doctype == ANY_DOCTYPE || self.doctype == doctype) && self.actions.contains(&action)
    }
}

/// What a key may do. Pure data; holds no right the owner lacks (see
/// [`check_creation`](KeyScope::check_creation) and [`decide`](KeyScope::decide)).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct KeyScope {
    /// The allow-list.
    pub allow: Vec<AllowRule>,
    /// An optional value restriction, the same type as an assignment's scope.
    #[serde(default)]
    pub scope: Vec<UserPermission>,
    /// When the key stops working (unix seconds), if it does.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// One right a key asks for and its owner does not hold.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Excess {
    /// The DocType asked for, or `"*"`.
    pub doctype: String,
    /// The action asked for.
    pub action: KeyAction,
    /// For `"*"`: the DocTypes the owner lacks the action on (empty for a named DocType).
    pub missing_on: Vec<String>,
}

/// One problem with a key scope.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// A right the owner does not hold.
    #[error("iamaccess.scope_excess: the key asks for `{}` on `{}`, which its owner does not hold", .0.action.as_str(), .0.doctype)]
    Excess(Excess),
    /// The scope allows nothing, or a rule names no action.
    #[error("iamaccess.scope_empty: the key scope allows nothing")]
    Empty,
    /// Only a service or internal user owns a key.
    #[error("iamaccess.key_owner_type: only a service or internal user can own an API key")]
    OwnerType,
    /// The owner is disabled or anonymised.
    #[error("iamaccess.key_owner_inactive: the owner of an API key must be an enabled user")]
    OwnerInactive,
}

impl ScopeError {
    /// The stable `iamaccess.*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ScopeError::Excess(_) => SCOPE_EXCESS,
            ScopeError::Empty => SCOPE_EMPTY,
            ScopeError::OwnerType => KEY_OWNER_TYPE,
            ScopeError::OwnerInactive => KEY_OWNER_INACTIVE,
        }
    }
}

/// Every problem found, never just the first.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct ScopeErrors(pub Vec<ScopeError>);

impl ScopeErrors {
    /// The excess grants among the errors.
    #[must_use]
    pub fn excesses(&self) -> Vec<&Excess> {
        self.0
            .iter()
            .filter_map(|e| match e {
                ScopeError::Excess(x) => Some(x),
                _ => None,
            })
            .collect()
    }

    /// The codes, in order.
    #[must_use]
    pub fn codes(&self) -> Vec<&'static str> {
        self.0.iter().map(ScopeError::code).collect()
    }
}

impl fmt::Display for ScopeErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Id(id) => id.to_string(),
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

impl KeyScope {
    /// Whether the owner holds `action` on `doctype` right now. The probe document is one the
    /// owner created, so an `if_owner` grant counts: the key is still narrowed by the owner's
    /// decision on each real document.
    fn owner_holds(
        engine: &PermissionEngine,
        owner: &Principal,
        doctype: &str,
        action: KeyAction,
    ) -> bool {
        let probe = DocFacts {
            doctype: doctype.to_owned(),
            created_by: Some(owner.user.clone()),
            ..DocFacts::default()
        };
        engine.can(owner, action.to_action(), &probe).allowed
    }

    /// Proves the scope is a subset of the owner's rights **now**. Every `(doctype, action)`
    /// the key asks for must be one the owner holds; the error lists every excess, not the
    /// first. A wildcard DocType is allowed only if the owner holds the action on every
    /// DocType the snapshot knows.
    ///
    /// # Errors
    ///
    /// [`ScopeErrors`] listing every excess grant and an empty scope.
    pub fn check_creation(
        &self,
        owner: &Principal,
        engine: &PermissionEngine,
    ) -> Result<(), ScopeErrors> {
        let mut errors: Vec<ScopeError> = Vec::new();
        if self.allow.is_empty() || self.allow.iter().any(|r| r.actions.is_empty()) {
            errors.push(ScopeError::Empty);
        }
        let known: Vec<&str> = engine.snapshot().doctypes().collect();
        for rule in &self.allow {
            for action in &rule.actions {
                if rule.doctype == ANY_DOCTYPE {
                    let missing: Vec<String> = known
                        .iter()
                        .filter(|d| !Self::owner_holds(engine, owner, d, *action))
                        .map(|d| (*d).to_owned())
                        .collect();
                    if known.is_empty() || !missing.is_empty() {
                        errors.push(ScopeError::Excess(Excess {
                            doctype: ANY_DOCTYPE.to_owned(),
                            action: *action,
                            missing_on: missing,
                        }));
                    }
                } else if !Self::owner_holds(engine, owner, &rule.doctype, *action) {
                    errors.push(ScopeError::Excess(Excess {
                        doctype: rule.doctype.clone(),
                        action: *action,
                        missing_on: Vec::new(),
                    }));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ScopeErrors(errors))
        }
    }

    /// The `(field path, value)` pairs the key's value restriction blocks on this document.
    fn scope_blocks(&self, engine: &PermissionEngine, doc: &DocFacts) -> Vec<(String, String)> {
        let Some(facts) = engine.snapshot().facts(&doc.doctype) else {
            return Vec::new();
        };
        let mut restrictions: Vec<&UserPermission> = self.scope.iter().collect();
        restrictions.sort();
        let mut out = Vec::new();
        for up in restrictions {
            if up.doctype.as_deref().is_some_and(|d| d != doc.doctype) {
                continue;
            }
            for link in &facts.links {
                let Some(value) = doc.values.get(&link.name) else {
                    continue;
                };
                let target = link.target.clone().unwrap_or_default();
                let path = format!("{}.{}", doc.doctype, link.name);
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

    /// The key's decision: the owner's decision AND the key's allow-list AND the value
    /// restriction, with the reasons of all three. Field rights are the owner's; the key adds
    /// none.
    #[must_use]
    pub fn decide(
        &self,
        engine: &PermissionEngine,
        owner: &Principal,
        action: Action,
        doc: &DocFacts,
    ) -> Decision {
        let mut decision = engine.can(owner, action, doc);
        let key_action = KeyAction::from_action(action);
        if !self
            .allow
            .iter()
            .any(|r| r.covers(&doc.doctype, key_action))
        {
            decision.allowed = false;
            decision.reasons.push(Reason::NoGrant {
                doctype: doc.doctype.clone(),
                wanted: format!("key:{}", key_action.as_str()),
            });
        }
        for (field, value) in self.scope_blocks(engine, doc) {
            decision.allowed = false;
            decision.reasons.push(Reason::UserPermissionBlocked {
                assignment: "api-key".to_owned(),
                field,
                value,
            });
        }
        decision
    }
}
