//! Shared fixture: the spec, the approvers and the approved grant every test starts
//! from.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_support_session::Tier;
use spot_support_session::model::{
    ActionScope, DocTypeScope, Scope, SessionPolicy, SessionSpec, Subject, SubjectKind,
};

pub const TENANT: &str = "acme";
pub const START: i64 = 1_700_000_000;
pub const END: i64 = START + 3600;

/// A valid named-scope spec at `T1`, with a canonical reason.
pub fn spec() -> SessionSpec {
    SessionSpec {
        id: "ss-0001".to_owned(),
        tenant: TENANT.to_owned(),
        subject: Subject::new(SubjectKind::Agent, "agent-support-7"),
        tier_ceiling: Tier::T1,
        scope: Scope {
            doctypes: DocTypeScope::Named(vec![
                "Sales Invoice".to_owned(),
                "Payment Entry".to_owned(),
            ]),
            actions: ActionScope::Any,
        },
        reason: "customer reports stuck invoices".to_owned(),
        requested_by: "support-desk".to_owned(),
        starts_at: START,
        expires_at: END,
    }
}

/// The tenant owner on the customer side.
pub fn owner() -> spot_support_session::Approver {
    spot_support_session::Approver::new("owner-ops", true)
}

/// A plain approver (no tenant owner flag), different from the requester.
pub fn approver() -> spot_support_session::Approver {
    spot_support_session::Approver::new("customer-admin", false)
}

/// An approved `T1` grant with a `Named` scope and `Any` actions.
pub fn grant() -> spot_support_session::SessionGrant {
    spot_support_session::SessionGrant::request(&spec(), &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0
}
