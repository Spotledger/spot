//! The tenant's governance policy: who may grant what, and which grants need approval
//! (ADR-070 section 8 rule 2 and section 10 rule 3).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_iam_roles::RoleName;
use spot_sod::Duty;

use crate::error::GovError;
use crate::target::EscalationTarget;

/// Whether a grantor is limited to the roles and scopes they hold themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationPolicy {
    /// A grantor grants only roles and scopes they hold themselves (ADR-070 section 10,
    /// rule 3: "a company's finance lead grants roles inside that company only"). The default.
    #[default]
    WithinHeld,
    /// Any administrator with the grant right may grant any role in any scope.
    Unrestricted,
}

/// Which roles and duties need approval before a grant takes effect, and the approval's shape
/// (ADR-070 section 8, rule 2: "a tenant can require approval for chosen roles or duties").
///
/// The approval runs on the BPM engine; this policy only says *that* an approval is needed and
/// who is asked, never *how* the workflow runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ApprovalPolicy {
    /// Roles that always need approval.
    #[serde(default)]
    pub roles: BTreeSet<RoleName>,
    /// Duties that always need approval: a role that expands to one of them needs approval
    /// even when the role itself is not listed.
    #[serde(default)]
    pub duties: BTreeSet<Duty>,
    /// Who is asked first: a user name or a `role:` reference the BPM task layer resolves.
    #[serde(default)]
    pub approver: String,
    /// Who the SLA escalates to. Absent until the policy is routed; a stored value that is not
    /// a valid [`EscalationTarget`] fails the whole load.
    #[serde(default)]
    pub escalate_to: Option<EscalationTarget>,
    /// How long the approver has before the approval escalates, in whole seconds.
    #[serde(default)]
    pub sla_secs: u64,
}

impl ApprovalPolicy {
    /// An empty policy: nothing needs approval.
    #[must_use]
    pub fn none() -> ApprovalPolicy {
        ApprovalPolicy::default()
    }

    /// Adds a role that needs approval.
    #[must_use]
    pub fn requiring_role(mut self, role: &str) -> ApprovalPolicy {
        self.roles.insert(RoleName::from(role));
        self
    }

    /// Adds a duty that needs approval.
    #[must_use]
    pub fn requiring_duty(mut self, duty: &str) -> ApprovalPolicy {
        if let Some(duty) = Duty::new(duty) {
            self.duties.insert(duty);
        }
        self
    }

    /// Sets who is asked and who the SLA escalates to, for `sla_secs` seconds.
    ///
    /// An `escalate_to` that is not a valid [`EscalationTarget`] leaves the policy with no
    /// escalation target, and the gate then refuses any grant this policy requires approval
    /// for (`GovError::RequestInvalid`): fail closed, never an approval that
    /// escalates to nobody. Use [`ApprovalPolicy::try_routed_to`] to learn about the bad value
    /// at build time instead.
    #[must_use]
    pub fn routed_to(mut self, approver: &str, escalate_to: &str, sla_secs: u64) -> ApprovalPolicy {
        self.approver = approver.to_owned();
        self.escalate_to = EscalationTarget::new(escalate_to).ok();
        self.sla_secs = sla_secs;
        self
    }

    /// As [`ApprovalPolicy::routed_to`], refusing an invalid escalation target.
    ///
    /// # Errors
    ///
    /// [`GovError::RequestInvalid`] when `escalate_to` is empty, blank, has a control
    /// character or is too long.
    pub fn try_routed_to(
        mut self,
        approver: &str,
        escalate_to: &str,
        sla_secs: u64,
    ) -> Result<ApprovalPolicy, GovError> {
        self.escalate_to = Some(EscalationTarget::new(escalate_to)?);
        self.approver = approver.to_owned();
        self.sla_secs = sla_secs;
        Ok(self)
    }

    /// Whether the policy asks for approval for a role, or for any duty the role expands to
    /// through `duties_of`.
    #[must_use]
    pub fn requires(&self, role: &RoleName, duties_of: impl IntoIterator<Item = Duty>) -> bool {
        self.roles.contains(role) || duties_of.into_iter().any(|d| self.duties.contains(&d))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_matches_a_role_or_a_duty_it_expands_to() {
        let policy = ApprovalPolicy::none()
            .requiring_role("Finance Approver")
            .requiring_duty("Approve payment run");
        assert!(policy.requires(&RoleName::from("Finance Approver"), std::iter::empty()));
        assert!(policy.requires(
            &RoleName::from("Accounts Payable Clerk"),
            [Duty::new("Approve payment run").expect("duty")]
        ));
        assert!(!policy.requires(
            &RoleName::from("Accounts Payable Clerk"),
            std::iter::empty()
        ));
    }

    #[test]
    fn delegation_defaults_to_within_held() {
        assert_eq!(DelegationPolicy::default(), DelegationPolicy::WithinHeld);
    }
}
