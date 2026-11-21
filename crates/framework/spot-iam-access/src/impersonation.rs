//! Impersonation by a support session (ADR-070 section 9, rule 2).
//!
//! A support principal with a customer-approved grant that covers impersonation may act as an
//! ordinary enabled user of the tenant. The effective principal is the target's own
//! assignments minus every forbidden role: `Tenant Administrator`, every break-glass role, any
//! role that inherits one of those, and every assignment that came from emergency access.
//! Nothing is ever added: the grant's scope is not a right. Both identities are recorded in
//! every event and audit record of the session.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_events_contract::Actor;
use spot_iam_roles::Registry;
use spot_permissions::{AssignmentContext, Principal, TENANT_ADMINISTRATOR};
use spot_support_session::{ActionRequest, DerivedStatus, SessionGrant, SessionStatus, Tier};

use crate::error::{IMPERSONATION_EXPIRED, IMPERSONATION_GRANT_INVALID, IMPERSONATION_NOT_ALLOWED};

/// The action name a grant must cover for impersonation to begin.
pub const IMPERSONATE_ACTION: &str = "impersonate";

/// The class of a principal, as far as impersonation cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalClass {
    /// The platform's support tier.
    Support,
    /// A tenant administrator.
    TenantAdministrator,
    /// Any other user of the tenant.
    Ordinary,
}

/// Who asks to impersonate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SupportPrincipal {
    /// The support person's id (the grant's subject).
    pub id: String,
    /// Their class; only [`PrincipalClass::Support`] may begin.
    pub class: PrincipalClass,
}

/// The user to be impersonated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpersonationTarget {
    /// The user's id.
    pub id: String,
    /// The tenant the user belongs to.
    pub tenant: String,
    /// The user's class.
    pub class: PrincipalClass,
    /// Whether the user is enabled.
    pub enabled: bool,
    /// The user's own assignments, as the permission engine sees them.
    pub principal: Principal,
    /// The ids of the assignments that came from emergency access (`source = emergency`).
    pub emergency_assignments: BTreeSet<String>,
}

/// What may not be carried into an impersonation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpersonationPolicy {
    /// Roles stripped by name. `Tenant Administrator` is always stripped, even if removed here.
    pub forbidden_roles: BTreeSet<String>,
    /// Roles flagged break-glass: stripped like the forbidden roles.
    pub break_glass_roles: BTreeSet<String>,
    /// For each role, every role it inherits from (transitively). See
    /// [`with_registry`](ImpersonationPolicy::with_registry).
    pub ancestors: BTreeMap<String, BTreeSet<String>>,
    /// The longest an impersonation lasts from its start, in seconds.
    pub max_duration_secs: i64,
    /// The support tier an impersonation counts as when the grant is checked.
    pub required_tier: Tier,
}

impl Default for ImpersonationPolicy {
    fn default() -> ImpersonationPolicy {
        ImpersonationPolicy {
            forbidden_roles: BTreeSet::from([TENANT_ADMINISTRATOR.to_owned()]),
            break_glass_roles: BTreeSet::new(),
            ancestors: BTreeMap::new(),
            max_duration_secs: 3600,
            required_tier: Tier::T1,
        }
    }
}

impl ImpersonationPolicy {
    /// This policy with the role inheritance of `registry`, so a role that inherits a
    /// forbidden role (directly or through other roles) is stripped too.
    #[must_use]
    pub fn with_registry(mut self, registry: &Registry) -> ImpersonationPolicy {
        let parents: BTreeMap<String, Vec<String>> = registry
            .roles()
            .map(|r| {
                (
                    r.name.0.clone(),
                    r.parents.iter().map(|p| p.0.clone()).collect(),
                )
            })
            .collect();
        for name in parents.keys() {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut stack: Vec<&String> = parents.get(name).into_iter().flatten().collect();
            while let Some(p) = stack.pop() {
                if seen.insert(p.clone()) {
                    stack.extend(parents.get(p).into_iter().flatten());
                }
            }
            self.ancestors.insert(name.clone(), seen);
        }
        self
    }

    /// This policy with these roles flagged break-glass.
    #[must_use]
    pub fn with_break_glass(mut self, roles: impl IntoIterator<Item = String>) -> Self {
        self.break_glass_roles.extend(roles);
        self
    }

    fn is_forbidden(&self, role: &str) -> Option<StripReason> {
        if role == TENANT_ADMINISTRATOR {
            return Some(StripReason::TenantAdministrator);
        }
        if self.break_glass_roles.contains(role) {
            return Some(StripReason::BreakGlass);
        }
        if self.forbidden_roles.contains(role) {
            return Some(StripReason::Forbidden);
        }
        None
    }

    fn strip_reason(&self, role: &str) -> Option<StripReason> {
        if let Some(reason) = self.is_forbidden(role) {
            return Some(reason);
        }
        self.ancestors.get(role).and_then(|ancestors| {
            ancestors
                .iter()
                .find(|a| self.is_forbidden(a).is_some())
                .map(|a| StripReason::InheritsForbidden {
                    ancestor: a.clone(),
                })
        })
    }
}

/// Why a role was left out of the effective principal.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripReason {
    /// It is `Tenant Administrator`.
    TenantAdministrator,
    /// It is flagged break-glass.
    BreakGlass,
    /// The policy forbids it.
    Forbidden,
    /// It inherits a forbidden role.
    InheritsForbidden {
        /// The forbidden role it inherits.
        ancestor: String,
    },
    /// Its assignment came from emergency access.
    EmergencyAssignment,
}

/// A role left out of the effective principal, so the screen can say so.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Stripped {
    /// The assignment it was in.
    pub assignment: String,
    /// The role.
    pub role: String,
    /// Why.
    pub reason: StripReason,
}

/// The detail of a refusal, for the audit trail and tests. The error a caller shows says only
/// that the principal may not impersonate this user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImpersonationRefusal {
    /// The caller is not of the support tier.
    NotSupportTier,
    /// The caller is the target.
    SelfImpersonation,
    /// The target is another support principal.
    TargetIsSupport,
    /// The target is disabled.
    TargetDisabled,
    /// The target belongs to another tenant than the grant.
    WrongTenant,
    /// The target is the system principal.
    TargetIsSystem,
    /// The grant names another subject.
    GrantSubjectMismatch,
    /// The grant is not approved.
    GrantNotApproved,
    /// The grant was approved by the person it is for.
    GrantSelfApproved,
    /// The grant was revoked.
    GrantRevoked,
    /// The grant is not active at this time.
    GrantNotActive,
    /// The grant does not cover impersonation; the support-session refusal code says why.
    GrantDoesNotCover(&'static str),
}

/// Why an impersonation cannot begin or continue.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImpersonationError {
    /// This principal may not impersonate this user.
    #[error("iamaccess.impersonation_not_allowed: this principal may not impersonate this user")]
    NotAllowed(ImpersonationRefusal),
    /// The support grant does not allow impersonation now.
    #[error(
        "iamaccess.impersonation_grant_invalid: the support session does not allow impersonation now"
    )]
    GrantInvalid(ImpersonationRefusal),
    /// The impersonation is over.
    #[error("iamaccess.impersonation_expired: the impersonation session is over")]
    Expired,
}

impl ImpersonationError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ImpersonationError::NotAllowed(_) => IMPERSONATION_NOT_ALLOWED,
            ImpersonationError::GrantInvalid(_) => IMPERSONATION_GRANT_INVALID,
            ImpersonationError::Expired => IMPERSONATION_EXPIRED,
        }
    }

    /// The refusal detail, when there is one.
    #[must_use]
    pub fn refusal(&self) -> Option<&ImpersonationRefusal> {
        match self {
            ImpersonationError::NotAllowed(r) | ImpersonationError::GrantInvalid(r) => Some(r),
            ImpersonationError::Expired => None,
        }
    }
}

/// Both identities of an impersonation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The real person: the support principal.
    pub acting: String,
    /// The identity whose rights applied: the target user.
    pub as_user: String,
}

impl Identity {
    /// The identity in the form the events contract carries: `id` is the user whose rights
    /// applied and `via` is the real person. `None` when either id is not a record id.
    #[must_use]
    pub fn actor(&self) -> Option<Actor> {
        let id = spot_permissions::ids::parse_record_id(&self.as_user).ok()?;
        let via = spot_permissions::ids::parse_record_id(&self.acting).ok()?;
        Some(Actor::User { id, via: Some(via) })
    }
}

/// What every audit record of the session must include.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpersonationTrail {
    /// The real person.
    pub acting: String,
    /// The identity whose rights applied.
    pub as_user: String,
    /// The support grant that allowed it.
    pub grant_id: String,
    /// The tenant.
    pub tenant: String,
    /// When it began (unix seconds).
    pub started_at: i64,
    /// When it ends (unix seconds, exclusive).
    pub valid_until: i64,
    /// The roles left out, as `assignment:role`.
    pub stripped_roles: Vec<String>,
}

/// A running impersonation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impersonated {
    support_id: String,
    as_user: String,
    grant_id: String,
    tenant: String,
    started_at: i64,
    valid_until: i64,
    principal: Principal,
    stripped: Vec<Stripped>,
}

/// Entry point: [`Impersonation::begin`].
#[derive(Clone, Copy, Debug)]
pub struct Impersonation;

impl Impersonation {
    /// Begins an impersonation of `target` by `support` under `grant` at `now` (unix seconds).
    ///
    /// # Errors
    ///
    /// [`ImpersonationError::NotAllowed`] when the caller is not support tier, targets self,
    /// another support principal, a disabled user or another tenant;
    /// [`ImpersonationError::GrantInvalid`] when the grant is not approved, not active, revoked
    /// or does not cover impersonation.
    pub fn begin(
        grant: &SessionGrant,
        support: &SupportPrincipal,
        target: &ImpersonationTarget,
        policy: &ImpersonationPolicy,
        now: i64,
    ) -> Result<Impersonated, ImpersonationError> {
        use ImpersonationRefusal as R;
        let denied = |r: R| Err(ImpersonationError::NotAllowed(r));
        let grant_bad = |r: R| Err(ImpersonationError::GrantInvalid(r));
        if support.class != PrincipalClass::Support {
            return denied(R::NotSupportTier);
        }
        if support.id == target.id {
            return denied(R::SelfImpersonation);
        }
        if target.class == PrincipalClass::Support {
            return denied(R::TargetIsSupport);
        }
        if target.principal.is_system {
            return denied(R::TargetIsSystem);
        }
        if !target.enabled {
            return denied(R::TargetDisabled);
        }
        if target.tenant != grant.tenant {
            return denied(R::WrongTenant);
        }
        if grant.subject.id != support.id {
            return grant_bad(R::GrantSubjectMismatch);
        }
        if grant.is_revoked() || grant.status == SessionStatus::Revoked {
            return grant_bad(R::GrantRevoked);
        }
        if grant.status == SessionStatus::Pending || grant.approved_by.trim().is_empty() {
            return grant_bad(R::GrantNotApproved);
        }
        if grant.approved_by == support.id {
            return grant_bad(R::GrantSelfApproved);
        }
        if grant.status == SessionStatus::Expired || grant.status_at(now) != DerivedStatus::Active {
            return grant_bad(R::GrantNotActive);
        }
        let request = ActionRequest {
            action: IMPERSONATE_ACTION.to_owned(),
            tier: policy.required_tier,
            doctype: None,
            tenant: target.tenant.clone(),
        };
        if let Err(refusal) = grant.check(&request, now) {
            return grant_bad(R::GrantDoesNotCover(refusal.code()));
        }

        let (principal, stripped) = effective_principal(target, policy);
        let valid_until = grant
            .expires_at
            .min(now.saturating_add(policy.max_duration_secs.max(0)));
        Ok(Impersonated {
            support_id: support.id.clone(),
            as_user: target.id.clone(),
            grant_id: grant.id.clone(),
            tenant: target.tenant.clone(),
            started_at: now,
            valid_until,
            principal,
            stripped,
        })
    }
}

/// The target's assignments minus everything forbidden, and what was stripped. Never adds a
/// role, never sets the system flag.
fn effective_principal(
    target: &ImpersonationTarget,
    policy: &ImpersonationPolicy,
) -> (Principal, Vec<Stripped>) {
    let mut stripped: Vec<Stripped> = Vec::new();
    let mut assignments: Vec<AssignmentContext> = Vec::new();
    for assignment in &target.principal.assignments {
        let emergency = target.emergency_assignments.contains(&assignment.id);
        let mut kept = assignment.clone();
        kept.roles.clear();
        for role in &assignment.roles {
            let reason = if emergency {
                Some(StripReason::EmergencyAssignment)
            } else {
                policy.strip_reason(role.as_str())
            };
            match reason {
                Some(reason) => stripped.push(Stripped {
                    assignment: assignment.id.clone(),
                    role: role.as_str().to_owned(),
                    reason,
                }),
                None => kept.roles.push(role.clone()),
            }
        }
        if !kept.roles.is_empty() {
            assignments.push(kept);
        }
    }
    stripped.sort();
    (
        Principal {
            user: target.principal.user.clone(),
            assignments,
            is_system: false,
        },
        stripped,
    )
}

impl Impersonated {
    /// Both identities.
    #[must_use]
    pub fn identity(&self) -> Identity {
        Identity {
            acting: self.support_id.clone(),
            as_user: self.as_user.clone(),
        }
    }

    /// What every audit record of the session must include.
    #[must_use]
    pub fn trail(&self) -> ImpersonationTrail {
        ImpersonationTrail {
            acting: self.support_id.clone(),
            as_user: self.as_user.clone(),
            grant_id: self.grant_id.clone(),
            tenant: self.tenant.clone(),
            started_at: self.started_at,
            valid_until: self.valid_until,
            stripped_roles: self
                .stripped
                .iter()
                .map(|s| format!("{}:{}", s.assignment, s.role))
                .collect(),
        }
    }

    /// The principal the engine evaluates: the target's assignments minus the stripped roles.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The roles left out, so the screen can say so.
    #[must_use]
    pub fn stripped(&self) -> &[Stripped] {
        &self.stripped
    }

    /// When the impersonation ends (unix seconds, exclusive).
    #[must_use]
    pub fn valid_until(&self) -> i64 {
        self.valid_until
    }

    /// Each request calls this.
    ///
    /// # Errors
    ///
    /// [`ImpersonationError::Expired`] once `now` reaches `valid_until`.
    pub fn check(&self, now: i64) -> Result<(), ImpersonationError> {
        if now >= self.valid_until {
            Err(ImpersonationError::Expired)
        } else {
            Ok(())
        }
    }
}
