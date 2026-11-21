//! The errors of this crate: stable `iamassign.*` codes, each with the subject it is about.
//!
//! Messages name roles, rules, groups and assignments by name or id, never a personal value.

use std::fmt;

/// `iamassign.bad_expression`: a rule's condition or scope value does not parse.
pub const BAD_EXPRESSION: &str = "iamassign.bad_expression";
/// `iamassign.condition_not_boolean`: a rule's condition can never be true or false.
pub const CONDITION_NOT_BOOLEAN: &str = "iamassign.condition_not_boolean";
/// `iamassign.scope_not_link`: a scope template value can never be a link value.
pub const SCOPE_NOT_LINK: &str = "iamassign.scope_not_link";
/// `iamassign.duplicate_rule`: two rules share an id.
pub const DUPLICATE_RULE: &str = "iamassign.duplicate_rule";
/// `iamassign.invalid_rule`: a rule or mapping has a bad id, name or duration.
pub const INVALID_RULE: &str = "iamassign.invalid_rule";
/// `iamassign.unknown_role`: a rule or mapping names a role that does not exist.
pub const UNKNOWN_ROLE: &str = "iamassign.unknown_role";
/// `iamassign.emergency_role`: a rule or mapping would assign an emergency-only role.
pub const EMERGENCY_ROLE: &str = "iamassign.emergency_role";
/// `iamassign.eval_failed`: a rule could not be evaluated for one user; nothing was changed.
pub const EVAL_FAILED: &str = "iamassign.eval_failed";
/// `iamassign.idp_managed`: a manual change to a role the identity provider manages.
pub const IDP_MANAGED: &str = "iamassign.idp_managed";
/// `iamassign.not_owner`: a source tried to change an assignment of another source.
pub const NOT_OWNER: &str = "iamassign.not_owner";
/// `iamassign.source_edit`: an edit tried to change an assignment's source.
pub const SOURCE_EDIT: &str = "iamassign.source_edit";
/// `iamassign.emergency_end`: an emergency assignment can only be ended by the emergency
/// process or by expiry.
pub const EMERGENCY_END: &str = "iamassign.emergency_end";
/// `iamassign.wrong_source`: an assignment was added under a source other than the actor's.
pub const WRONG_SOURCE: &str = "iamassign.wrong_source";

/// One problem: a stable code, the thing it is about, and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignError {
    /// The stable, machine-readable code (`iamassign.*`).
    pub code: &'static str,
    /// The rule, mapping, role or assignment the problem is about, when there is one.
    pub subject: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl AssignError {
    /// A problem about no particular subject.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            subject: None,
            message: message.into(),
        }
    }

    /// A problem about `subject`.
    pub fn about(code: &'static str, subject: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            subject: Some(subject.to_owned()),
            message: message.into(),
        }
    }
}

impl fmt::Display for AssignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.subject {
            Some(subject) => write!(f, "{} (`{subject}`): {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for AssignError {}

/// Every problem found while loading, never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignErrors(Vec<AssignError>);

impl AssignErrors {
    /// The problems, or `None` for an empty list.
    pub fn from_list(list: Vec<AssignError>) -> Option<Self> {
        (!list.is_empty()).then_some(Self(list))
    }

    /// The problems, in the order found.
    pub fn errors(&self) -> &[AssignError] {
        &self.0
    }

    /// The codes, in the order found.
    pub fn codes(&self) -> Vec<&'static str> {
        self.0.iter().map(|e| e.code).collect()
    }

    /// Whether a problem with `code` is among them.
    pub fn has(&self, code: &str) -> bool {
        self.0.iter().any(|e| e.code == code)
    }
}

impl fmt::Display for AssignErrors {
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

impl std::error::Error for AssignErrors {}
