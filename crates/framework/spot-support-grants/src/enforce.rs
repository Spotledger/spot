//! The enforcement decision: may this session run this action, now?

use serde::{Deserialize, Serialize};

use spot_support_session::{ActionRequest as SessionAction, Tier};

use crate::error::codes;
use crate::model::Grant;
use crate::store::GrantStore;

/// Which grant a running session works under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    /// The tenant the session is about.
    pub tenant: String,
    /// The grant id the session was opened under.
    pub grant_id: String,
    /// The support organisation the session claims.
    pub support_org: String,
    /// The id of the subject (agent or person) acting in the session. It must be the
    /// grant's subject: a session that knows the grant id but is run by anybody else is
    /// refused (`support_grants.subject_mismatch`).
    pub subject_id: String,
}

/// One action a session asks to run.
///
/// Identical in shape to the session model's request, with one addition: the plain-language
/// change the action performed, which is what the customer reads in "what changed". The
/// message arguments themselves never enter the record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    /// The action's name (`read`, `update`, `repair_workflow_state`, a tool name).
    pub action: String,
    /// The tier the action needs, from the action catalogue.
    pub tier: Tier,
    /// The DocType the action reaches, or `None` for a tenant-wide action.
    pub doctype: Option<String>,
    /// The tenant the action is about.
    pub tenant: String,
    /// What the action changed, in plain words; empty for reads.
    #[serde(default)]
    pub change: String,
}

impl Action {
    /// A tenant-wide action at `tier`.
    #[must_use]
    pub fn new(action: &str, tier: Tier, tenant: &str) -> Action {
        Action {
            action: action.to_owned(),
            tier,
            doctype: None,
            tenant: tenant.to_owned(),
            change: String::new(),
        }
    }

    /// The same action reaching `doctype`.
    #[must_use]
    pub fn on(mut self, doctype: &str) -> Action {
        self.doctype = Some(doctype.to_owned());
        self
    }

    /// The same action, with the plain-language change it made.
    #[must_use]
    pub fn changing(mut self, change: &str) -> Action {
        self.change = change.to_owned();
        self
    }

    fn session_request(&self) -> SessionAction {
        SessionAction {
            action: self.action.clone(),
            tier: self.tier,
            doctype: self.doctype.clone(),
            tenant: self.tenant.clone(),
        }
    }
}

/// Why a check refused, with the stable code and the customer-readable sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// The stable code: a `session.*` code when the session model refused, a
    /// `support_grants.*` code otherwise.
    pub code: &'static str,
    /// The plain sentence, safe to show the customer.
    pub reason: String,
}

/// The answer to one action question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The grant covers the action, inside its tier, scope and window.
    Allow,
    /// The first failing reason refused it.
    Deny(Denial),
}

impl Decision {
    /// Whether the action may run.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    /// The stable outcome code: `support_grants.allowed`, a `session.*` refusal code, or a
    /// `support_grants.*` refusal code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Decision::Allow => codes::ALLOWED,
            Decision::Deny(denial) => denial.code,
        }
    }

    /// The customer-readable sentence.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Decision::Allow => "the grant covers the action, within its tier, scope and window",
            Decision::Deny(denial) => &denial.reason,
        }
    }
}

/// The enforcement function used by the control plane and the runtime.
///
/// It looks the grant up in the tenant's store, checks the session's tenant and support
/// organisation, and then asks the session model
/// ([`spot_support_session::SessionGrant::check`]), whose fixed refusal order is preserved.
/// The function is pure: it reads no clock (time is `now`) and appends nothing. The control
/// plane wraps it in [`crate::ControlPlane::attempt`], which records the outcome on the
/// customer side — a denial is always logged there, never silently dropped.
#[must_use]
pub fn check(grants: &dyn GrantStore, session: &SessionRef, action: &Action, now: i64) -> Decision {
    if session.tenant != action.tenant {
        return deny(
            codes::WRONG_TENANT,
            format!(
                "the action is about tenant `{}`, the session is for `{}`",
                action.tenant, session.tenant
            ),
        );
    }
    let grant = match grants.get(&session.tenant, &session.grant_id) {
        Ok(Some(grant)) => grant,
        Ok(None) => {
            return deny(
                codes::UNKNOWN_GRANT,
                format!(
                    "no grant `{}` exists for tenant `{}`",
                    session.grant_id, session.tenant
                ),
            );
        }
        Err(error) => return deny(error.code(), error.to_string()),
    };
    if let Some(denial) = check_grant(&grant, session, action, now) {
        return Denial::into_decision(denial);
    }
    Decision::Allow
}

/// The grant-level checks, shared by [`check`] and the control plane.
pub(crate) fn check_grant(
    grant: &Grant,
    session: &SessionRef,
    action: &Action,
    now: i64,
) -> Option<Denial> {
    if grant.tenant() != session.tenant {
        return Some(Denial {
            code: codes::WRONG_TENANT,
            reason: format!(
                "the grant covers tenant `{}`, the session is for `{}`",
                grant.tenant(),
                session.tenant
            ),
        });
    }
    if grant.support_org != session.support_org {
        return Some(Denial {
            code: codes::SUPPORT_ORG_MISMATCH,
            reason: format!(
                "the grant is made to `{}`, the session claims `{}`",
                grant.support_org, session.support_org
            ),
        });
    }
    if grant.subject().id != session.subject_id {
        return Some(Denial {
            code: codes::SUBJECT_MISMATCH,
            reason: format!(
                "the grant is made to `{}`, the session is run by `{}`",
                grant.subject().id,
                session.subject_id
            ),
        });
    }
    grant
        .session
        .check(&action.session_request(), now)
        .err()
        .map(|refusal| Denial {
            code: refusal.code(),
            reason: refusal.to_string(),
        })
}

fn deny(code: &'static str, reason: String) -> Decision {
    Decision::Deny(Denial { code, reason })
}

impl Denial {
    fn into_decision(self) -> Decision {
        Decision::Deny(self)
    }
}
