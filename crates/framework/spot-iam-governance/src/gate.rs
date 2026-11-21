//! The grant gate: the one decision an assignment creation passes before it takes effect
//! (ADR-070 section 8, rule 1).
//!
//! [`GrantGate::evaluate`] combines, in order:
//!
//! 1. **Delegated administration** — a grantor grants only roles and scopes they hold
//!    themselves, and never a manual change to an IdP-managed role (ADR-070 section 10 rule 3
//!    and section 4, through `spot-iam-assign`).
//! 2. **Segregation of duties** — the `spot-sod` grant-time check over the user's resulting
//!    duties in the resulting scope, before the assignment takes effect. A conflict needs an
//!    override: a grantor with the `override_sod` right, a reason and a mitigating control.
//! 3. **Approval** — a tenant policy that requires approval for the role or one of its duties.
//!
//! Before any of them, **the portal rule** (ADR-083 Barrier 3): a grant of a role to a portal
//! user is refused unless the role is a portal role whose every duty is marked
//! outside-exposed. It covers every source (manual, rule, IdP and the emergency grant, which is
//! this gate with `Source::Emergency`) and no override, approval or delegation policy lifts it.
//! The portal users come from [`GovernanceState::outside`]. The host builds that guard from the
//! registry and the user directory (`OutsideGuard::new`); `spot-iam-questions::build` is the
//! other consumer of the same list. A state cannot be built without stating its guard
//! ([`GovernanceState::new`] takes it; there is no `Default`), so forgetting it is a compile
//! error, not a silent hole.
//!
//! The steps are sequential: a blocked grant is not offered an approval, and an overridden
//! conflict still faces the approval policy.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_iam_assign::{Actor, ChangeRequest, ManagedRoles, authorise_change};
use spot_iam_roles::{Assignment, OutsideGuard};
use spot_sod::{Grant, GrantCheck, Role, RoleCatalogue, RuleSet, SodOverride, check_grant};

use crate::approval::ApprovalSpec;
use crate::conflict::{Conflict, Refusal, sort_conflicts};
use crate::error::{
    DELEGATION_DENIED, GovError, IDP_MANAGED, OVERRIDE_INCOMPLETE, OVERRIDE_NOT_PERMITTED,
};
use crate::override_record::{SodOverrideRecord, record_overrides};
use crate::policy::{ApprovalPolicy, DelegationPolicy};
use crate::request::GrantRequest;
use crate::scope::{GrantScope, ScopeHierarchy};

/// The tenant-wide governance data one grant is evaluated against.
///
/// There is deliberately no `Default` and no argument-less constructor: the portal guard must be
/// stated.
///
/// ```compile_fail
/// let _ = spot_iam_governance::GovernanceState::default();
/// ```
///
/// ```compile_fail
/// let _ = spot_iam_governance::GovernanceState::new();
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GovernanceState {
    /// The SoD rules the grant is checked against.
    pub rules: RuleSet,
    /// The role catalogue that expands roles to duties.
    pub catalogue: RoleCatalogue,
    /// The active assignments, all users. A grantor's own assignments are a subset of these.
    pub assignments: Vec<Assignment>,
    /// The recorded overrides, which mark the conflicts they cover as already tolerated.
    pub overrides: Vec<SodOverrideRecord>,
    /// Whether a grantor is limited to the roles and scopes they hold.
    pub delegation: DelegationPolicy,
    /// Which roles and duties need approval.
    pub approval: ApprovalPolicy,
    /// The parent/child relation over scope values, for "include descendants".
    pub hierarchy: ScopeHierarchy,
    /// The roles the tenant declared IdP-managed.
    pub managed_roles: ManagedRoles,
    /// The portal users, and which roles they may hold (ADR-083 Barrier 3). Empty protects no
    /// one.
    pub outside: OutsideGuard,
}

impl GovernanceState {
    /// A state with no rules, no assignments and no policy (delegation `WithinHeld`), guarding
    /// the portal users `outside` lists. The guard is required so a host cannot forget it.
    #[must_use]
    pub fn new(outside: OutsideGuard) -> GovernanceState {
        GovernanceState {
            rules: RuleSet::default(),
            catalogue: RoleCatalogue::default(),
            assignments: Vec::new(),
            overrides: Vec::new(),
            delegation: DelegationPolicy::default(),
            approval: ApprovalPolicy::default(),
            hierarchy: ScopeHierarchy::default(),
            managed_roles: ManagedRoles::default(),
            outside,
        }
    }

    /// A state that protects **no portal user**: the portal rule never fires. For tests that are
    /// not about the portal rule and for tenants that have no portal users. A host with even one
    /// portal user must not call this; it must call [`GovernanceState::new`] with its guard.
    #[must_use]
    pub fn without_portal_users() -> GovernanceState {
        GovernanceState::new(OutsideGuard::none())
    }

    /// Sets the SoD rules.
    #[must_use]
    pub fn with_rules(mut self, rules: RuleSet) -> GovernanceState {
        self.rules = rules;
        self
    }

    /// Sets the role catalogue.
    #[must_use]
    pub fn with_catalogue(mut self, catalogue: RoleCatalogue) -> GovernanceState {
        self.catalogue = catalogue;
        self
    }

    /// Sets the active assignments.
    #[must_use]
    pub fn with_assignments(mut self, assignments: Vec<Assignment>) -> GovernanceState {
        self.assignments = assignments;
        self
    }

    /// Sets the recorded overrides.
    #[must_use]
    pub fn with_overrides(mut self, overrides: Vec<SodOverrideRecord>) -> GovernanceState {
        self.overrides = overrides;
        self
    }

    /// Sets the delegation policy.
    #[must_use]
    pub fn with_delegation(mut self, delegation: DelegationPolicy) -> GovernanceState {
        self.delegation = delegation;
        self
    }

    /// Sets the approval policy.
    #[must_use]
    pub fn with_approval(mut self, approval: ApprovalPolicy) -> GovernanceState {
        self.approval = approval;
        self
    }

    /// Sets the scope hierarchy.
    #[must_use]
    pub fn with_hierarchy(mut self, hierarchy: ScopeHierarchy) -> GovernanceState {
        self.hierarchy = hierarchy;
        self
    }

    /// Sets the IdP-managed roles.
    #[must_use]
    pub fn with_managed_roles(mut self, managed_roles: ManagedRoles) -> GovernanceState {
        self.managed_roles = managed_roles;
        self
    }

    /// Replaces the portal-user guard given to [`GovernanceState::new`], for a host that
    /// refreshes the portal-user list on an existing state.
    #[must_use]
    pub fn with_outside_guard(mut self, outside: OutsideGuard) -> GovernanceState {
        self.outside = outside;
        self
    }

    /// The recorded overrides as the SoD core sees them.
    #[must_use]
    pub fn sod_overrides(&self) -> Vec<SodOverride> {
        self.overrides
            .iter()
            .map(SodOverrideRecord::to_sod_override)
            .collect()
    }
}

/// What the gate decided about one grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDecision {
    /// The grant may proceed: no conflict, no refusal, no approval needed (or the conflict is
    /// already overridden by the request's complete submission).
    Allow,
    /// The grant is refused; every reason is listed, not just the first. A refusal is never
    /// overridable.
    Blocked(Vec<Conflict>),
    /// The grant needs approval on the BPM engine before it takes effect. The spec always
    /// carries a valid escalation target.
    NeedsApproval(ApprovalSpec),
    /// The grant creates segregation-of-duties conflicts the grantor may override: they hold
    /// the `override_sod` right but have not yet given a reason and a mitigating control.
    NeedsOverride(Vec<Conflict>),
}

impl GrantDecision {
    /// Whether the grant may proceed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, GrantDecision::Allow)
    }

    /// The conflicts the decision carries, when it carries any.
    #[must_use]
    pub fn conflicts(&self) -> &[Conflict] {
        match self {
            GrantDecision::Blocked(c) | GrantDecision::NeedsOverride(c) => c,
            GrantDecision::Allow | GrantDecision::NeedsApproval(_) => &[],
        }
    }
}

/// The grant-time gate (ADR-070 section 8, rule 1). Stateless: everything it reads is in the
/// [`GrantRequest`] and the [`GovernanceState`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GrantGate;

impl GrantGate {
    /// A gate.
    #[must_use]
    pub fn new() -> GrantGate {
        GrantGate
    }

    /// Decides a grant. See the module documentation for the order of the checks.
    ///
    /// # Errors
    ///
    /// [`crate::error::GovError::RequestInvalid`] when the tenant's approval policy requires
    /// approval for this grant but carries no valid escalation target: the gate
    /// fails closed rather than reporting an approval nobody could decide. The error names
    /// the request and the rule, never an offered value.
    pub fn evaluate(
        &self,
        request: &GrantRequest,
        state: &GovernanceState,
    ) -> Result<GrantDecision, GovError> {
        // A blank user, role or grantor must never reach the checks below: an empty role has no
        // duties, so it would pass the SoD check and be allowed (fail-open).
        if let Err(error) = request.validate() {
            return Ok(GrantDecision::Blocked(vec![Conflict::Refusal(
                Refusal::new(error.code(), request.id.as_str(), &error.to_string()),
            )]));
        }
        let refusals = self.delegation_refusals(request, state);
        if !refusals.is_empty() {
            return Ok(GrantDecision::Blocked(refusals));
        }
        let conflicts = self.sod_conflicts(request, state);
        if !conflicts.is_empty() {
            match &request.override_submission {
                None if request.grantor.may_override_sod() => {
                    return Ok(GrantDecision::NeedsOverride(conflicts));
                }
                None => return Ok(GrantDecision::Blocked(conflicts)),
                Some(_) if request.grantor.user.trim() == request.user.trim() => {
                    // Nobody overrides a conflict on their own access: that is self-approval.
                    return Ok(GrantDecision::Blocked(vec![Conflict::Refusal(
                        Refusal::new(
                            OVERRIDE_NOT_PERMITTED,
                            request.id.as_str(),
                            "a user may not override a segregation-of-duties conflict on their own access",
                        ),
                    )]));
                }
                Some(_) if !request.grantor.may_override_sod() => {
                    return Ok(GrantDecision::Blocked(vec![Conflict::Refusal(
                        Refusal::new(
                            OVERRIDE_NOT_PERMITTED,
                            request.id.as_str(),
                            "recording an SoD override needs the `override_sod` right",
                        ),
                    )]));
                }
                Some(submission) if !submission.is_complete() => {
                    return Ok(GrantDecision::Blocked(vec![Conflict::Refusal(
                        Refusal::new(
                            OVERRIDE_INCOMPLETE,
                            request.id.as_str(),
                            "an override needs a non-blank reason and a mitigating control with an \
                         id and text",
                        ),
                    )]));
                }
                Some(_) => {}
            }
        }
        if let Some(spec) = self.approval_spec(request, state)? {
            return Ok(GrantDecision::NeedsApproval(spec));
        }
        Ok(GrantDecision::Allow)
    }

    /// The segregation-of-duties conflicts the grant would create, excluding those an existing
    /// override already covers. Sorted deterministically.
    #[must_use]
    pub fn sod_conflicts(&self, request: &GrantRequest, state: &GovernanceState) -> Vec<Conflict> {
        let existing: Vec<spot_sod::Assignment> = state
            .assignments
            .iter()
            .flat_map(|a| sod_assignments(a, &state.hierarchy))
            .collect();
        let overrides = state.sod_overrides();
        let mut seen: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
        let mut out: Vec<Conflict> = Vec::new();
        for proposed in proposed_assignments(request, &state.hierarchy) {
            if let GrantCheck::Conflicts(conflicts) = check_grant(
                &state.rules,
                &state.catalogue,
                &existing,
                &proposed,
                &overrides,
            ) {
                for conflict in conflicts {
                    if conflict.overridden {
                        continue;
                    }
                    let key = (
                        conflict.rule_id.clone(),
                        conflict.user.clone(),
                        conflict.scope.clone(),
                    );
                    if seen.insert(key) {
                        out.push(Conflict::Sod(conflict));
                    }
                }
            }
        }
        sort_conflicts(&mut out);
        out
    }

    /// The delegation refusals for the grant, led by the portal rule (ADR-083 Barrier 3): the
    /// grantor does not hold the role, does not hold
    /// it in the requested scope, or the role is IdP-managed. Empty when the policy is
    /// [`DelegationPolicy::Unrestricted`] and no source rule is broken.
    #[must_use]
    pub fn delegation_refusals(
        &self,
        request: &GrantRequest,
        state: &GovernanceState,
    ) -> Vec<Conflict> {
        let mut out: Vec<Conflict> = Vec::new();
        if let Err(errors) = state.outside.check(&request.user, &request.role) {
            for error in errors.errors() {
                out.push(Conflict::Refusal(Refusal::new(
                    error.code,
                    request.role.as_str(),
                    &error.message,
                )));
            }
            // A refusal for a portal user is final: nothing else can make it right.
            sort_conflicts(&mut out);
            return out;
        }
        let proposed = assignment_from_request(request);
        if let Err(error) = authorise_change(
            &ChangeRequest::Add(proposed),
            Actor::Source(request.source),
            &state.managed_roles,
        ) {
            let code = if error.code == spot_iam_assign::codes::IDP_MANAGED {
                IDP_MANAGED
            } else {
                DELEGATION_DENIED
            };
            out.push(Conflict::Refusal(Refusal::new(
                code,
                request.role.as_str(),
                &error.to_string(),
            )));
        }
        if state.delegation == DelegationPolicy::Unrestricted {
            sort_conflicts(&mut out);
            return out;
        }
        let held: Vec<&Assignment> = state
            .assignments
            .iter()
            .filter(|a| a.user == request.grantor.user && a.role == request.role)
            .collect();
        if held.is_empty() {
            out.push(Conflict::Refusal(Refusal::new(
                DELEGATION_DENIED,
                request.role.as_str(),
                &format!(
                    "the grantor `{}` does not hold the role they are granting",
                    request.grantor.user
                ),
            )));
        } else if !held
            .iter()
            .any(|a| GrantScope::from_scope(&a.scope).covers(&request.scope, &state.hierarchy))
        {
            out.push(Conflict::Refusal(Refusal::new(
                DELEGATION_DENIED,
                request.role.as_str(),
                "the grantor does not hold the role in the requested scope",
            )));
        }
        sort_conflicts(&mut out);
        out
    }

    /// The approval spec for the grant, when the tenant's policy requires approval.
    ///
    /// # Errors
    ///
    /// [`GovError::RequestInvalid`] (code `iam_governance.request_invalid`) when the policy
    /// requires approval but has no valid escalation target: a grant that needs
    /// approval must say who the SLA escalates to, so the rule is refused at gate time
    /// instead of producing a spec that can never be decided. The detail names the rule and
    /// never the offered value.
    pub fn approval_spec(
        &self,
        request: &GrantRequest,
        state: &GovernanceState,
    ) -> Result<Option<ApprovalSpec>, GovError> {
        let duties = Role::new(request.role.as_str())
            .map(|role| state.catalogue.duties_of(&role))
            .unwrap_or_default();
        if !state.approval.requires(&request.role, duties) {
            return Ok(None);
        }
        let Some(escalate_to) = state.approval.escalate_to.clone() else {
            return Err(GovError::RequestInvalid {
                subject: request.id.as_str().to_owned(),
                detail: "the approval policy requires approval but has no valid escalation \
                         target: who the SLA escalates to must be an actor id or \
                         `role:<name>`"
                    .to_owned(),
            });
        };
        Ok(Some(ApprovalSpec {
            request: request.id.clone(),
            user: request.user.clone(),
            role: request.role.clone(),
            scope: request.scope.clone(),
            reason: format!("the grant of `{}` needs approval", request.role.as_str()),
            approver: state.approval.approver.clone(),
            escalate_to,
            sla_secs: state.approval.sla_secs,
        }))
    }

    /// Builds the auditable override records for the conflicts a request's submission covers.
    /// See [`record_overrides`].
    ///
    /// # Errors
    ///
    /// As [`record_overrides`].
    pub fn record_overrides(
        &self,
        request: &GrantRequest,
        conflicts: &[Conflict],
        at: i64,
    ) -> Result<Vec<SodOverrideRecord>, GovError> {
        record_overrides(request, conflicts, at)
    }
}

/// The assignment a request becomes, for the source/ownership check.
fn assignment_from_request(request: &GrantRequest) -> Assignment {
    let mut assignment = Assignment::new(
        &request.id.assignment_id(),
        &request.user,
        request.role.as_str(),
    );
    assignment.scope = request.scope.to_scope();
    assignment.source = request.source;
    assignment.reason = request.reason.clone();
    assignment
}

/// The `spot-sod` assignments an assignment expands to: one per scope token.
fn sod_assignments(
    assignment: &Assignment,
    hierarchy: &ScopeHierarchy,
) -> Vec<spot_sod::Assignment> {
    let Some(role) = Role::new(assignment.role.as_str()) else {
        return Vec::new();
    };
    GrantScope::from_scope(&assignment.scope)
        .tokens(hierarchy)
        .into_iter()
        .map(|scope| spot_sod::Assignment {
            user: assignment.user.clone(),
            grant: Grant::Role(role.clone()),
            scope,
        })
        .collect()
}

/// The `spot-sod` assignments a proposed grant expands to: one per scope token.
fn proposed_assignments(
    request: &GrantRequest,
    hierarchy: &ScopeHierarchy,
) -> Vec<spot_sod::Assignment> {
    let Some(role) = Role::new(request.role.as_str()) else {
        return Vec::new();
    };
    request
        .scope
        .tokens(hierarchy)
        .into_iter()
        .map(|scope| spot_sod::Assignment {
            user: request.user.clone(),
            grant: Grant::Role(role.clone()),
            scope,
        })
        .collect()
}
