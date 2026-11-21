//! The control-plane workflow: request, approve, start, act, stop, revoke and extend, with
//! every step recorded on the customer's own chain.

use spot_agent_tiers::{AgentRegistry, Domain, Status, Tier as AgentTier};
use spot_support_session::{DerivedStatus, Subject, SubjectKind, Tier};

use crate::customer::CustomerAdmins;
use crate::enforce::{Action, Decision, Denial, SessionRef, check};
use crate::error::{GrantError, codes};
use crate::model::{Grant, GrantSpec, PendingGrant, scope_summary};
use crate::policy::GrantPolicy;
use crate::record::{CustomerLog, EventKind, GrantEvent};
use crate::store::GrantStore;

/// The control plane's grant workflow.
///
/// It owns the customer's approval directory, the agent registry, the deployment policy and
/// the tenant's log, and it is the only path that writes grants: a request is checked
/// against the agent registry, an approval against the customer's administrators, and every
/// transition and action is appended to the tenant's chain before success is reported.
///
/// Time is always an argument; nothing here reads a clock.
pub struct ControlPlane<'a> {
    grants: &'a mut dyn GrantStore,
    log: &'a mut dyn CustomerLog,
    admins: &'a CustomerAdmins,
    agents: &'a AgentRegistry,
    policy: &'a GrantPolicy,
}

impl<'a> ControlPlane<'a> {
    /// A control plane over the tenant's grant store and log.
    pub fn new(
        grants: &'a mut dyn GrantStore,
        log: &'a mut dyn CustomerLog,
        admins: &'a CustomerAdmins,
        agents: &'a AgentRegistry,
        policy: &'a GrantPolicy,
    ) -> ControlPlane<'a> {
        ControlPlane {
            grants,
            log,
            admins,
            agents,
            policy,
        }
    }

    /// Requests a grant for an agent or person and records the request.
    ///
    /// An agent subject must be enrolled, active, in the support domain, and its maximum
    /// tier must be at least the requested ceiling. The request is recorded but conveys no
    /// authority until a customer administrator approves it.
    ///
    /// # Errors
    ///
    /// [`GrantError::AgentRefused`], the request's validation errors, or
    /// [`GrantError::AuditUnavailable`].
    pub fn request(&mut self, spec: GrantSpec, now: i64) -> Result<PendingGrant, GrantError> {
        check_agent(
            self.agents,
            &spec.session.subject,
            spec.session.tier_ceiling,
        )?;
        let pending = Grant::request(&spec, self.policy, now)?;
        let mut event = lifecycle(
            EventKind::Requested,
            &spec.session.tenant,
            &spec.session.id,
            &spec.session.requested_by,
            SubjectKind::Person,
            &spec.session.subject.id,
            spec.session.subject.kind,
            &spec.support_org,
            spec.session.tier_ceiling,
        );
        event.scope = scope_summary(&spec.session.scope);
        event.outcome = "requested".to_owned();
        event.code = codes::REQUESTED.to_owned();
        event.reason = spec.session.reason.clone();
        event.at = now;
        self.record(event)?;
        Ok(pending)
    }

    /// Approves a pending grant on the customer's behalf and stores it.
    ///
    /// # Errors
    ///
    /// [`GrantError::NotAdmin`], [`GrantError::SelfApproval`], the session approval
    /// refusal, [`GrantError::DuplicateGrant`], or [`GrantError::AuditUnavailable`].
    pub fn approve(
        &mut self,
        pending: PendingGrant,
        approver_id: &str,
        now: i64,
    ) -> Result<Grant, GrantError> {
        let tenant = pending.tenant().to_owned();
        let support_org = pending.support_org().to_owned();
        let tier = pending.tier();
        let scope = scope_summary(pending.scope());
        let (grant, _event) = pending.approve(self.admins, approver_id, now, self.policy)?;
        if self.grants.get(&tenant, &grant.id)?.is_some() {
            return Err(GrantError::DuplicateGrant {
                tenant,
                id: grant.id.clone(),
            });
        }
        let mut event = lifecycle(
            EventKind::Approved,
            &tenant,
            &grant.id,
            approver_id,
            SubjectKind::Person,
            &grant.subject().id,
            grant.subject().kind,
            &support_org,
            tier,
        );
        event.scope = scope;
        event.outcome = "approved".to_owned();
        event.code = codes::APPROVED.to_owned();
        event.reason = format!("approved by {approver_id}");
        event.at = now;
        // Fail closed: a grant that conveys authority exists only once the customer's own
        // chain has recorded its approval.
        self.record(event)?;
        self.grants.insert(grant.clone())?;
        Ok(grant)
    }

    /// Starts a session under a grant, after checking it is running at `now`.
    ///
    /// # Errors
    ///
    /// [`GrantError::UnknownGrant`], [`GrantError::NotRunning`], or
    /// [`GrantError::AuditUnavailable`].
    pub fn start_session(&mut self, tenant: &str, id: &str, now: i64) -> Result<(), GrantError> {
        let grant = self.load(tenant, id)?;
        if !grant.is_running(now) {
            return Err(GrantError::NotRunning {
                id: id.to_owned(),
                now,
                state: state_text(&grant, now),
            });
        }
        let mut event = lifecycle(
            EventKind::Started,
            tenant,
            id,
            &grant.subject().id,
            grant.subject().kind,
            &grant.subject().id,
            grant.subject().kind,
            &grant.support_org,
            grant.tier(),
        );
        event.scope = scope_summary(grant.scope());
        event.outcome = "started".to_owned();
        event.code = codes::STARTED.to_owned();
        event.at = now;
        self.record(event)
    }

    /// Stops a session and records the stop with its reason.
    ///
    /// # Errors
    ///
    /// [`GrantError::UnknownGrant`] or [`GrantError::AuditUnavailable`].
    pub fn stop_session(
        &mut self,
        tenant: &str,
        id: &str,
        reason: &str,
        now: i64,
    ) -> Result<(), GrantError> {
        let grant = self.load(tenant, id)?;
        let mut event = lifecycle(
            EventKind::Stopped,
            tenant,
            id,
            &grant.subject().id,
            grant.subject().kind,
            &grant.subject().id,
            grant.subject().kind,
            &grant.support_org,
            grant.tier(),
        );
        event.scope = scope_summary(grant.scope());
        event.outcome = "stopped".to_owned();
        event.code = codes::STOPPED.to_owned();
        event.reason = reason.trim().to_owned();
        event.at = now;
        self.record(event)
    }

    /// Runs the enforcement check and records the outcome on the customer side.
    ///
    /// A denial is recorded too, so an out-of-scope or expired attempt is visible to the
    /// customer and not only in the vendor's logs. If an allowed action cannot be recorded
    /// (the tenant's chain is unavailable), it is refused instead: the platform fails
    /// closed rather than act without a customer-visible record.
    pub fn attempt(&mut self, session: &SessionRef, action: &Action, now: i64) -> Decision {
        let decision = check(self.grants, session, action, now);
        let grant = self
            .grants
            .get(&session.tenant, &session.grant_id)
            .ok()
            .flatten();
        let (who, who_kind, tier, scope) = match &grant {
            Some(grant) => (
                grant.subject().id.clone(),
                grant.subject().kind,
                action.tier,
                scope_summary(grant.scope()),
            ),
            None => (
                "unknown".to_owned(),
                SubjectKind::Agent,
                action.tier,
                String::new(),
            ),
        };
        let mut event = lifecycle(
            EventKind::Action,
            &session.tenant,
            &session.grant_id,
            &who,
            who_kind,
            &who,
            who_kind,
            &session.support_org,
            tier,
        );
        event.scope = scope;
        event.action = action.action.clone();
        event.doctype = action.doctype.clone();
        event.outcome = if decision.is_allowed() {
            "allowed".to_owned()
        } else {
            "denied".to_owned()
        };
        event.code = decision.code().to_owned();
        event.change = action.change.clone();
        event.reason = decision.reason().to_owned();
        event.at = now;
        let recorded = self.record(event).is_ok();
        if decision.is_allowed() && !recorded {
            return Decision::Deny(Denial {
                code: codes::AUDIT_UNAVAILABLE,
                reason:
                    "the customer-side audit chain could not record the action, so it was refused"
                        .to_owned(),
            });
        }
        decision
    }

    /// Revokes a grant at the customer's request (or the support side's), immediately.
    ///
    /// The grant stays in the store as evidence; a new grant is the only way back.
    ///
    /// # Errors
    ///
    /// [`GrantError::UnknownGrant`], [`GrantError::AlreadyRevoked`], or
    /// [`GrantError::AuditUnavailable`].
    pub fn revoke(
        &mut self,
        tenant: &str,
        id: &str,
        by: &str,
        reason: &str,
        now: i64,
    ) -> Result<(), GrantError> {
        let mut grant = self.load(tenant, id)?;
        if grant.is_revoked() {
            return Err(GrantError::AlreadyRevoked(id.to_owned()));
        }
        grant.session.revoke(by, reason, now)?;
        self.grants.replace(grant.clone())?;
        let mut event = lifecycle(
            EventKind::Revoked,
            tenant,
            id,
            by,
            SubjectKind::Person,
            &grant.subject().id,
            grant.subject().kind,
            &grant.support_org,
            grant.tier(),
        );
        event.scope = scope_summary(grant.scope());
        event.outcome = "revoked".to_owned();
        event.code = codes::REVOKED.to_owned();
        event.reason = reason.trim().to_owned();
        event.at = now;
        self.record(event)
    }

    /// Extends a grant's end as a **new approval** by a customer administrator.
    ///
    /// The extension cannot push the total duration from the original start past the
    /// configured cap, cannot revive an expired or revoked grant, and stays in the grant's
    /// history.
    ///
    /// # Errors
    ///
    /// [`GrantError::NotAdmin`], [`GrantError::SelfApproval`],
    /// [`GrantError::DurationOverPolicy`], [`GrantError::ExtensionRefused`], or
    /// [`GrantError::AuditUnavailable`].
    pub fn extend(
        &mut self,
        tenant: &str,
        id: &str,
        new_expires_at: i64,
        approver_id: &str,
        now: i64,
    ) -> Result<(), GrantError> {
        let mut grant = self.load(tenant, id)?;
        let approver = self
            .admins
            .approver(tenant, approver_id)
            .ok_or_else(|| GrantError::NotAdmin(approver_id.to_owned()))?;
        if approver_id == grant.session.requested_by || approver_id == grant.subject().id {
            return Err(GrantError::SelfApproval(approver_id.to_owned()));
        }
        let total = new_expires_at.saturating_sub(grant.session.starts_at);
        if total > self.policy.duration_cap() {
            return Err(GrantError::DurationOverPolicy {
                asked: total,
                max: self.policy.duration_cap(),
            });
        }
        grant
            .session
            .extend(new_expires_at, &approver, now, &self.policy.session)
            .map_err(|error| GrantError::ExtensionRefused(error.to_string()))?;
        let mut event = lifecycle(
            EventKind::Extended,
            tenant,
            id,
            approver_id,
            SubjectKind::Person,
            &grant.subject().id,
            grant.subject().kind,
            &grant.support_org,
            grant.tier(),
        );
        event.scope = scope_summary(grant.scope());
        event.outcome = "extended".to_owned();
        event.code = codes::EXTENDED.to_owned();
        event.reason = format!("extended to {new_expires_at} by {approver_id}");
        event.extended_to = Some(new_expires_at);
        event.at = now;
        // Fail closed: the longer window takes effect only after the customer's chain
        // recorded the extension.
        self.record(event)?;
        self.grants.replace(grant)
    }

    fn load(&self, tenant: &str, id: &str) -> Result<Grant, GrantError> {
        self.grants
            .get(tenant, id)?
            .ok_or_else(|| GrantError::UnknownGrant {
                tenant: tenant.to_owned(),
                id: id.to_owned(),
            })
    }

    fn record(&mut self, event: GrantEvent) -> Result<(), GrantError> {
        let tenant = event.tenant.clone();
        if self.log.tenant() != tenant {
            return Err(GrantError::WrongTenant {
                asked: tenant,
                grant: self.log.tenant().to_owned(),
            });
        }
        self.log.append(event)
    }
}

#[allow(clippy::too_many_arguments)]
fn lifecycle(
    kind: EventKind,
    tenant: &str,
    session: &str,
    who: &str,
    who_kind: SubjectKind,
    subject: &str,
    subject_kind: SubjectKind,
    support_org: &str,
    tier: Tier,
) -> GrantEvent {
    GrantEvent {
        kind,
        tenant: tenant.to_owned(),
        session: session.to_owned(),
        who: who.to_owned(),
        who_kind,
        subject: subject.to_owned(),
        subject_kind,
        support_org: support_org.to_owned(),
        tier,
        scope: String::new(),
        action: String::new(),
        doctype: None,
        outcome: String::new(),
        code: String::new(),
        change: String::new(),
        reason: String::new(),
        extended_to: None,
        at: 0,
    }
}

fn check_agent(agents: &AgentRegistry, subject: &Subject, ceiling: Tier) -> Result<(), GrantError> {
    if subject.kind != SubjectKind::Agent {
        return Ok(());
    }
    let agent = agents
        .get(&subject.id)
        .ok_or_else(|| GrantError::AgentRefused {
            id: subject.id.clone(),
            detail: "the agent is not enrolled".to_owned(),
        })?;
    if agent.status != Status::Active {
        return Err(GrantError::AgentRefused {
            id: subject.id.clone(),
            detail: format!("the agent is {}", agent.status.as_str()),
        });
    }
    if agent.domain != Domain::Support {
        return Err(GrantError::AgentRefused {
            id: subject.id.clone(),
            detail: "the agent is not in the support domain".to_owned(),
        });
    }
    if agent.max_tier < agent_tier(ceiling) {
        return Err(GrantError::AgentRefused {
            id: subject.id.clone(),
            detail: format!(
                "the grant's {} ceiling is above the agent's {} maximum",
                ceiling.as_str(),
                agent.max_tier.as_str()
            ),
        });
    }
    Ok(())
}

fn agent_tier(tier: Tier) -> AgentTier {
    match tier {
        Tier::T0 => AgentTier::T0,
        Tier::T1 => AgentTier::T1,
        Tier::T2 => AgentTier::T2,
        Tier::T3 => AgentTier::T3,
    }
}

fn state_text(grant: &Grant, now: i64) -> String {
    if grant.is_revoked() {
        return "revoked".to_owned();
    }
    match grant.status_at(now) {
        DerivedStatus::NotYetActive => "not yet started".to_owned(),
        DerivedStatus::Active => "active".to_owned(),
        DerivedStatus::Expired => "expired".to_owned(),
    }
}
