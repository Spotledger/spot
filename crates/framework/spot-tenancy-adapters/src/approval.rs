//! The approval adapter: the tenant's deletion approvers, resolved from the control plane's
//! roles.
//!
//! The port answers with principals, and the control plane requires two distinct ones other than
//! the requester. This adapter therefore never invents an approver: it asks the policy for the
//! tenant's approver roles, expands each role to its members and returns the distinct,
//! non-empty principals. If the policy has no approver role for the tenant, or any lookup
//! fails, it answers with an empty list — the hook has no error channel, and an empty list is
//! exactly the "nobody approved" the plane refuses. It is never "anyone".

use std::collections::BTreeSet;

use spot_tenancy::Tenant;
use spot_tenancy::ports::ApprovalHook;

use crate::error::AdapterError;

/// Where a tenant's approver roles and their members come from. The caller supplies the
/// implementation over the control plane's role store; the role names are the ones
/// `spot-iam-roles` knows.
pub trait ApprovalPolicy: Send + Sync {
    /// The control-plane roles whose members may approve deleting `tenant`.
    ///
    /// # Errors
    /// [`AdapterError::Approval`] or [`AdapterError::NotConfigured`].
    fn approver_roles(&self, tenant: &Tenant) -> Result<Vec<String>, AdapterError>;

    /// The distinct principals holding `role`.
    ///
    /// # Errors
    /// [`AdapterError::Approval`] or [`AdapterError::NotConfigured`].
    fn role_members(&self, tenant: &Tenant, role: &str) -> Result<Vec<String>, AdapterError>;
}

/// An `&P` is an [`ApprovalPolicy`] too, so one policy can be shared.
impl<P: ApprovalPolicy + ?Sized> ApprovalPolicy for &P {
    fn approver_roles(&self, tenant: &Tenant) -> Result<Vec<String>, AdapterError> {
        (**self).approver_roles(tenant)
    }

    fn role_members(&self, tenant: &Tenant, role: &str) -> Result<Vec<String>, AdapterError> {
        (**self).role_members(tenant, role)
    }
}

/// [`ApprovalHook`](spot_tenancy::ports::ApprovalHook) over an [`ApprovalPolicy`].
pub struct RoleApprovals<P> {
    policy: P,
}

impl<P> RoleApprovals<P> {
    /// An adapter over `policy`.
    pub fn new(policy: P) -> Self {
        Self { policy }
    }

    /// The policy, for the caller that also reads it elsewhere.
    #[must_use]
    pub fn policy(&self) -> &P {
        &self.policy
    }
}

impl<P: ApprovalPolicy> RoleApprovals<P> {
    fn resolve(&self, tenant: &Tenant) -> Result<Vec<String>, AdapterError> {
        let roles = self.policy.approver_roles(tenant)?;
        if roles.is_empty() {
            // No configured role means no approver; an empty list is a refusal, never a licence
            // for anyone to approve.
            return Err(AdapterError::Approval);
        }
        let mut approvers = BTreeSet::new();
        for role in roles {
            for member in self.policy.role_members(tenant, &role)? {
                let member = member.trim();
                if !member.is_empty() {
                    approvers.insert(member.to_owned());
                }
            }
        }
        Ok(approvers.into_iter().collect())
    }
}

impl<P: ApprovalPolicy> ApprovalHook for RoleApprovals<P> {
    fn approvers(&self, tenant: &Tenant) -> Vec<String> {
        self.resolve(tenant).unwrap_or_default()
    }
}
