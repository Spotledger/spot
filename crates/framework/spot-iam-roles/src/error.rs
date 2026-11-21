//! The one error of this crate: a list of problems, each with a stable `iam.*` code.
//!
//! Validation reports every problem at once. Messages name roles, duties and assignments by
//! name or id, never a personal value.

use std::fmt;

/// `iam.invalid_name`: a role or duty name is empty, too long or has control characters.
pub const INVALID_NAME: &str = "iam.invalid_name";
/// `iam.duplicate_name`: two roles, or two duties, share a name.
pub const DUPLICATE_NAME: &str = "iam.duplicate_name";
/// `iam.grant_role`: a grant inside a duty or role names a different role than its owner.
pub const GRANT_ROLE: &str = "iam.grant_role";
/// `iam.unknown_duty`: a role names a duty that does not exist.
pub const UNKNOWN_DUTY: &str = "iam.unknown_duty";
/// `iam.duty_version`: a role pins a duty version the registry does not have.
pub const DUTY_VERSION: &str = "iam.duty_version";
/// `iam.unknown_parent`: a role inherits from a role that does not exist.
pub const UNKNOWN_PARENT: &str = "iam.unknown_parent";
/// `iam.unknown_role`: an assignment names a role that does not exist.
pub const UNKNOWN_ROLE: &str = "iam.unknown_role";
/// `iam.role_cycle`: role inheritance is not acyclic.
pub const ROLE_CYCLE: &str = "iam.role_cycle";
/// `iam.role_depth`: role inheritance is deeper than the limit.
pub const ROLE_DEPTH: &str = "iam.role_depth";
/// `iam.reference_readonly`: a reference role cannot be changed; copy it first.
pub const REFERENCE_READONLY: &str = "iam.reference_readonly";
/// `iam.duplicate_assignment`: two assignments are the same assignment.
pub const DUPLICATE_ASSIGNMENT: &str = "iam.duplicate_assignment";
/// `iam.assignment_window`: an assignment's `valid_to` is not after its `valid_from`.
pub const ASSIGNMENT_WINDOW: &str = "iam.assignment_window";

/// `iam_outside.role_kind`: a portal user was given a role that is not a portal role, or that
/// inherits from one that is not (ADR-083 Barrier 3).
pub const OUTSIDE_ROLE_KIND: &str = "iam_outside.role_kind";
/// `iam_outside.duty_not_exposed`: a portal user was given a role that reaches a duty (or a
/// direct right) not marked outside-exposed (ADR-083 Barrier 3).
pub const OUTSIDE_DUTY_NOT_EXPOSED: &str = "iam_outside.duty_not_exposed";
/// `iam_outside.role_edit_refused`: an edit to a role would let its portal holders reach
/// something an outside user may not.
pub const OUTSIDE_ROLE_EDIT_REFUSED: &str = "iam_outside.role_edit_refused";

/// One problem: a stable code, the thing it is about, and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IamError {
    /// The stable, machine-readable code (`iam.*`).
    pub code: &'static str,
    /// The role, duty or assignment the problem is about, when there is one.
    pub subject: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl IamError {
    /// A problem about no particular subject.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            subject: None,
            message: message.into(),
        }
    }

    /// A problem about `subject` (a role, duty or assignment id).
    pub fn about(code: &'static str, subject: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            subject: Some(subject.to_owned()),
            message: message.into(),
        }
    }
}

impl fmt::Display for IamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.subject {
            Some(subject) => write!(f, "{} (`{subject}`): {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

/// Every problem found, never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IamErrors(Vec<IamError>);

impl IamErrors {
    /// One problem.
    pub fn one(error: IamError) -> Self {
        Self(vec![error])
    }

    /// The problems, or `None` for an empty list.
    pub fn from_list(list: Vec<IamError>) -> Option<Self> {
        (!list.is_empty()).then_some(Self(list))
    }

    /// The problems, in the order found.
    pub fn errors(&self) -> &[IamError] {
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

impl fmt::Display for IamErrors {
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

impl std::error::Error for IamErrors {}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, IamErrors>;
