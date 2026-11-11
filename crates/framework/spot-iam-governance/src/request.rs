//! What a grant is: the request, its idempotent id, the grantor, and the override the grantor
//! may attach (ADR-070 section 8, rules 1 and 2).
//!
//! A [`GrantRequest`] is the whole proposed change: who receives which role, in which scope,
//! from which source, by whose hand, and — when the grantor chooses to override a
//! segregation-of-duties conflict — the reason and mitigating control that go with it.

use serde::{Deserialize, Serialize};
use spot_iam_roles::{RoleName, Source};
use spot_sod::Rights;

use crate::error::GovError;
use crate::scope::GrantScope;

/// The id of one grant request: the idempotency key of the whole approval and application.
///
/// The approval the BPM engine runs and the assignment the grant finally becomes are both
/// keyed by this id, so a retry after a crash never launches a second approval or writes a
/// second assignment (ADR-070 section 8, rule 2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantRequestId(String);

impl GrantRequestId {
    /// The id `text`, trimmed; `None` when it is empty or blank.
    #[must_use]
    pub fn new(text: &str) -> Option<GrantRequestId> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(GrantRequestId(trimmed.to_owned()))
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The assignment id this request deterministically becomes: the same request id always
    /// yields the same assignment id, so applying a grant is idempotent.
    #[must_use]
    pub fn assignment_id(&self) -> String {
        format!("grant:{}", self.0)
    }
}

/// Who is granting, and the rights the decision sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Grantor {
    /// The granting user's name.
    pub user: String,
    /// The rights the grantor holds, as the SoD override decision sees them (the
    /// `override_sod` right is the only one this crate reads).
    #[serde(default)]
    pub rights: Rights,
}

impl Grantor {
    /// A grantor with these rights.
    #[must_use]
    pub fn new(user: &str, rights: impl IntoIterator<Item = impl AsRef<str>>) -> Grantor {
        Grantor {
            user: user.to_owned(),
            rights: Rights::new(rights),
        }
    }

    /// A grantor holding no special rights.
    #[must_use]
    pub fn plain(user: &str) -> Grantor {
        Grantor {
            user: user.to_owned(),
            rights: Rights::default(),
        }
    }

    /// Whether the grantor holds the `override_sod` right.
    #[must_use]
    pub fn may_override_sod(&self) -> bool {
        self.rights.has_override_sod()
    }
}

/// The compensating control that makes an overridden conflict tolerable (ADR-070 section 8,
/// rule 1): a reference (an id naming a control record) and the text that describes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MitigatingControl {
    /// The control's id: what the audit trail and the access review point at.
    pub id: String,
    /// The control in words.
    pub text: String,
}

impl MitigatingControl {
    /// The control `id` described by `text`.
    #[must_use]
    pub fn new(id: &str, text: &str) -> MitigatingControl {
        MitigatingControl {
            id: id.to_owned(),
            text: text.to_owned(),
        }
    }

    /// Whether the control is complete: a non-blank id and a non-blank text.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.id.trim().is_empty() && !self.text.trim().is_empty()
    }
}

/// The override a grantor attaches to a conflicting grant: a reason and a mitigating control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct OverrideSubmission {
    /// Why the conflict is tolerated; required, non-blank after trimming.
    pub reason: String,
    /// The compensating control; required and complete.
    pub mitigating_control: MitigatingControl,
}

impl OverrideSubmission {
    /// An override with this reason and control.
    #[must_use]
    pub fn new(reason: &str, control: MitigatingControl) -> OverrideSubmission {
        OverrideSubmission {
            reason: reason.to_owned(),
            mitigating_control: control,
        }
    }

    /// Whether the submission is complete: a non-blank reason and a complete control.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.reason.trim().is_empty() && self.mitigating_control.is_complete()
    }
}

/// One proposed grant: the whole input of [`crate::GrantGate::evaluate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GrantRequest {
    /// The idempotent request id.
    pub id: GrantRequestId,
    /// Who the grant is for.
    pub user: String,
    /// The role being granted.
    pub role: RoleName,
    /// Where the role applies.
    #[serde(default)]
    pub scope: GrantScope,
    /// Where the assignment comes from (manual, rule, idp or emergency).
    pub source: Source,
    /// Why the grant is being made.
    #[serde(default)]
    pub reason: Option<String>,
    /// Who is granting it, and with which rights.
    pub grantor: Grantor,
    /// The override the grantor attaches, when they are overriding a conflict.
    #[serde(default)]
    pub override_submission: Option<OverrideSubmission>,
}

impl GrantRequest {
    /// A manual, unscoped, reasoned grant by a plain grantor.
    #[must_use]
    pub fn new(id: &GrantRequestId, user: &str, role: &str, grantor: Grantor) -> GrantRequest {
        GrantRequest {
            id: id.clone(),
            user: user.to_owned(),
            role: RoleName::from(role),
            scope: GrantScope::unscoped(),
            source: Source::Manual,
            reason: None,
            grantor,
            override_submission: None,
        }
    }

    /// Sets the scope.
    #[must_use]
    pub fn with_scope(mut self, scope: GrantScope) -> GrantRequest {
        self.scope = scope;
        self
    }

    /// Sets the source.
    #[must_use]
    pub fn with_source(mut self, source: Source) -> GrantRequest {
        self.source = source;
        self
    }

    /// Sets the reason.
    #[must_use]
    pub fn with_reason(mut self, reason: &str) -> GrantRequest {
        self.reason = Some(reason.to_owned());
        self
    }

    /// Attaches an override submission.
    #[must_use]
    pub fn with_override(mut self, submission: OverrideSubmission) -> GrantRequest {
        self.override_submission = Some(submission);
        self
    }

    /// Validates the request on its own: a non-blank user, role and grantor.
    ///
    /// # Errors
    ///
    /// [`GovError::RequestInvalid`] naming the first blank field.
    pub fn validate(&self) -> Result<(), GovError> {
        if self.user.trim().is_empty() {
            return Err(invalid(&self.id, "a grant needs a user"));
        }
        if self.role.as_str().trim().is_empty() {
            return Err(invalid(&self.id, "a grant needs a role"));
        }
        if self.grantor.user.trim().is_empty() {
            return Err(invalid(&self.id, "a grant needs a grantor"));
        }
        Ok(())
    }

    /// Whether the request's submission is present and complete.
    #[must_use]
    pub fn has_complete_override(&self) -> bool {
        self.override_submission
            .as_ref()
            .is_some_and(OverrideSubmission::is_complete)
    }
}

/// A [`GovError::RequestInvalid`] about one request.
pub(crate) fn invalid(id: &GrantRequestId, detail: &str) -> GovError {
    GovError::RequestInvalid {
        subject: id.as_str().to_owned(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_assignment_id_is_derived_from_the_request_id() {
        let id = GrantRequestId::new("req-1").expect("non-blank");
        assert_eq!(id.assignment_id(), "grant:req-1");
        assert_eq!(GrantRequestId::new("   "), None);
    }

    #[test]
    fn an_override_needs_a_reason_and_a_complete_control() {
        let complete = OverrideSubmission::new(
            "compensating review",
            MitigatingControl::new("C-1", "weekly review"),
        );
        assert!(complete.is_complete());
        let no_reason =
            OverrideSubmission::new("  ", MitigatingControl::new("C-1", "weekly review"));
        assert!(!no_reason.is_complete());
        let no_text = OverrideSubmission::new("why", MitigatingControl::new("C-1", " "));
        assert!(!no_text.is_complete());
    }

    #[test]
    fn a_plain_grantor_cannot_override() {
        assert!(!Grantor::plain("ana").may_override_sod());
        assert!(Grantor::new("ana", ["override_sod"]).may_override_sod());
    }
}
