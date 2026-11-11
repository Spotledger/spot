//! The grant record: a customer-approved, scoped, time-boxed support session, plus the
//! support organisation and the customer's conditions.

use serde::{Deserialize, Serialize};

use spot_support_session::{
    ActionScope, DerivedStatus, DocTypeScope, Scope, SessionEvent, SessionGrant, SessionSpec,
    Subject, SubjectKind, Tier,
};

use crate::customer::CustomerAdmins;
use crate::error::GrantError;
use crate::policy::GrantPolicy;

/// A support-session grant as the control plane stores it.
///
/// The grant is the brief's `Grant { tenant, support_org, agent_or_person, tier, scope,
/// valid_from, valid_until, approved_by, conditions }`: the tenant, subject, tier ceiling,
/// scope, window and approver live in [`session`](Self::session) (the existing
/// [`SessionGrant`]); this record adds the support organisation whose agent acts and the
/// customer's conditions.
///
/// There is no end-less grant: [`SessionGrant::expires_at`] is a mandatory signed instant,
/// so "permanent support access" cannot be expressed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Grant {
    /// The grant id; the same id as `session.id`.
    pub id: String,
    /// The support organisation the grant is made to.
    pub support_org: String,
    /// The conditions the customer attached (for example "read-only outside incidents"),
    /// in the customer's words; never blank when present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<String>,
    /// The session model's record: tenant, subject, tier ceiling, scope, window, approver,
    /// extensions and revocation.
    pub session: SessionGrant,
}

/// What a support requester asks the customer to approve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GrantSpec {
    /// The session request: id, tenant, subject, tier ceiling, scope, reason, requester and
    /// the half-open window `[starts_at, expires_at)`.
    pub session: SessionSpec,
    /// The support organisation the grant is made to.
    pub support_org: String,
    /// The conditions the customer will read before approving; optional.
    #[serde(default)]
    pub conditions: Vec<String>,
}

/// A requested grant, awaiting the customer's approval.
///
/// The only way to a [`Grant`]; none of the fields can be filled from data, so a stored
/// record cannot arrive approved.
#[derive(Clone, Debug)]
pub struct PendingGrant {
    pending: spot_support_session::PendingGrant,
    support_org: String,
    conditions: Vec<String>,
}

impl PendingGrant {
    /// The grant id the request names.
    #[must_use]
    pub fn id(&self) -> &str {
        self.pending.id()
    }

    /// The tenant whose administrators must approve.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.pending.grant().tenant
    }

    /// The subject who would act inside the session.
    #[must_use]
    pub fn subject(&self) -> &Subject {
        &self.pending.grant().subject
    }

    /// The requested tier ceiling.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.pending.grant().tier_ceiling
    }

    /// The support organisation the grant is made to.
    #[must_use]
    pub fn support_org(&self) -> &str {
        &self.support_org
    }

    /// The scope the session would reach.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.pending.grant().scope
    }

    /// The customer's conditions, as requested.
    #[must_use]
    pub fn conditions(&self) -> &[String] {
        &self.conditions
    }

    /// Approves the request on the customer's behalf.
    ///
    /// The approver must be a registered [`CustomerAdmins`] administrator of the tenant,
    /// must differ from both the requester and the subject (nobody approves their own
    /// access), and must satisfy the session policy (a ceiling above
    /// [`GrantPolicy::session`]'s `max_tier_without_owner_approval` needs a tenant owner).
    ///
    /// # Errors
    ///
    /// [`GrantError::NotAdmin`], [`GrantError::SelfApproval`], or the session model's
    /// approval refusal.
    pub fn approve(
        self,
        admins: &CustomerAdmins,
        approver_id: &str,
        now: i64,
        policy: &GrantPolicy,
    ) -> Result<(Grant, SessionEvent), GrantError> {
        let tenant = self.pending.grant().tenant.clone();
        let approver = admins
            .approver(&tenant, approver_id)
            .ok_or_else(|| GrantError::NotAdmin(approver_id.to_owned()))?;
        if approver_id == self.pending.grant().requested_by
            || approver_id == self.pending.grant().subject.id
        {
            return Err(GrantError::SelfApproval(approver_id.to_owned()));
        }
        let id = self.pending.id().to_owned();
        let (session, event) = self.pending.approve(&approver, now, &policy.session)?;
        let grant = Grant {
            id,
            support_org: self.support_org,
            conditions: self.conditions,
            session,
        };
        grant.validate()?;
        Ok((grant, event))
    }
}

impl Grant {
    /// Requests a grant: validates the control plane's own limits and asks the session
    /// model to validate the window, scope and tier.
    ///
    /// No support-agent identity is checked here; [`crate::ControlPlane::request`] composes
    /// that with the agent registry. This function is the pure record path.
    ///
    /// # Errors
    ///
    /// [`GrantError::InvalidRequest`] for a blank id, tenant, support organisation or
    /// condition, [`GrantError::DurationOverPolicy`], or the session model's request
    /// refusal.
    pub fn request(
        spec: &GrantSpec,
        policy: &GrantPolicy,
        now: i64,
    ) -> Result<PendingGrant, GrantError> {
        if spec.session.id.trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the grant id is blank".to_owned(),
            ));
        }
        if spec.session.tenant.trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the grant tenant is blank".to_owned(),
            ));
        }
        if spec.support_org.trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the support organisation is blank".to_owned(),
            ));
        }
        if spec.conditions.iter().any(|c| c.trim().is_empty()) {
            return Err(GrantError::InvalidRequest(
                "a condition is blank".to_owned(),
            ));
        }
        if spec.conditions.len() > policy.max_conditions {
            return Err(GrantError::InvalidRequest(format!(
                "the grant carries {} conditions, the policy allows {}",
                spec.conditions.len(),
                policy.max_conditions
            )));
        }
        let duration = spec
            .session
            .expires_at
            .saturating_sub(spec.session.starts_at);
        let cap = policy.duration_cap();
        if duration > cap {
            return Err(GrantError::DurationOverPolicy {
                asked: duration,
                max: cap,
            });
        }
        let pending = SessionGrant::request(&spec.session, &policy.session, now)?;
        Ok(PendingGrant {
            pending,
            support_org: spec.support_org.trim().to_owned(),
            conditions: spec
                .conditions
                .iter()
                .map(|c| c.trim().to_owned())
                .collect(),
        })
    }

    /// The tenant the grant covers.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.session.tenant
    }

    /// The subject who may act inside the session.
    #[must_use]
    pub fn subject(&self) -> &Subject {
        &self.session.subject
    }

    /// The tier ceiling nothing inside the session may rise above.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.session.tier_ceiling
    }

    /// The scope the session reaches.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.session.scope
    }

    /// The inclusive start instant (unix seconds).
    #[must_use]
    pub fn valid_from(&self) -> i64 {
        self.session.starts_at
    }

    /// The exclusive end instant (unix seconds); always present.
    #[must_use]
    pub fn valid_until(&self) -> i64 {
        self.session.expires_at
    }

    /// The customer administrator who approved the grant.
    #[must_use]
    pub fn approved_by(&self) -> &str {
        &self.session.approved_by
    }

    /// The grant's liveness at `now`, derived from its times only.
    #[must_use]
    pub fn status_at(&self, now: i64) -> DerivedStatus {
        self.session.status_at(now)
    }

    /// Whether the grant may act at `now`: approved, inside its window and not revoked.
    #[must_use]
    pub fn is_running(&self, now: i64) -> bool {
        !self.session.is_revoked() && self.status_at(now) == DerivedStatus::Active
    }

    /// Whether the customer revoked the grant.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.session.is_revoked()
    }

    /// The one plain sentence a customer reads in the grant list.
    #[must_use]
    pub fn summary(&self, now: i64) -> String {
        let conditions = if self.conditions.is_empty() {
            String::new()
        } else {
            format!("; conditions: {}", self.conditions.join("; "))
        };
        format!(
            "{} for tenant {} ({}){}",
            self.session.summary(now),
            self.tenant(),
            self.support_org,
            conditions
        )
    }

    /// Checks the record is internally consistent: the two ids agree, the tenant and
    /// support organisation are present, and no condition is blank.
    ///
    /// # Errors
    ///
    /// [`GrantError::InvalidRequest`] naming the first inconsistency.
    pub fn validate(&self) -> Result<(), GrantError> {
        if self.id != self.session.id {
            return Err(GrantError::InvalidRequest(
                "the grant id and the session id differ".to_owned(),
            ));
        }
        if self.tenant().trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the grant tenant is blank".to_owned(),
            ));
        }
        if self.support_org.trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the support organisation is blank".to_owned(),
            ));
        }
        if self.conditions.iter().any(|c| c.trim().is_empty()) {
            return Err(GrantError::InvalidRequest(
                "a condition is blank".to_owned(),
            ));
        }
        if self.session.approved_by.trim().is_empty() {
            return Err(GrantError::InvalidRequest(
                "the grant carries no customer approver".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A plain-language scope summary, used in grants and in the customer's record:
/// `"Factura, Payment; actions: read, update"`.
#[must_use]
pub fn scope_summary(scope: &Scope) -> String {
    let doctypes = match &scope.doctypes {
        DocTypeScope::Named(names) if names.is_empty() => "no DocType".to_owned(),
        DocTypeScope::Named(names) => names.join(", "),
        DocTypeScope::AllRead => "all DocTypes, read only".to_owned(),
    };
    let actions = match &scope.actions {
        ActionScope::Any => "any action".to_owned(),
        ActionScope::Only(actions) if actions.is_empty() => "no action".to_owned(),
        ActionScope::Only(actions) => actions.join(", "),
    };
    format!("{doctypes}; actions: {actions}")
}

/// The text form of a subject kind, as the customer reads it: `agent` or `person`.
#[must_use]
pub(crate) fn subject_kind_text(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Agent => "agent",
        SubjectKind::Person => "person",
    }
}

/// The audit actor form of a subject: `agent:<id>` or `user:<id>`.
#[must_use]
pub(crate) fn actor_text(kind: SubjectKind, id: &str) -> String {
    match kind {
        SubjectKind::Agent => format!("agent:{id}"),
        SubjectKind::Person => format!("user:{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_text_names_doctypes_and_actions() {
        let scope = Scope {
            doctypes: DocTypeScope::Named(vec!["Factura".to_owned()]),
            actions: ActionScope::Only(vec!["read".to_owned()]),
        };
        assert_eq!(scope_summary(&scope), "Factura; actions: read");
    }
}
