//! The decisions of the agent tier service: which tier a registration yields, which tool needs
//! which tier, and what a support session must allow. Pure functions over stored data, so the
//! hosting around them stays thin and every rule is a unit test.

use spot_agent_tiers::{Domain, Status, Tier};
use spot_support_session::{ActionRequest, DocTypeScope, Refusal, SessionGrant};

use super::store::Registration;

/// What a tool needs, derived from its name (`spot-mcp` names tools from metadata: `search_x`,
/// `get_x`, `create_x_draft`, `update_x`, `query`, `job_status`, `call_<app>_<module>_<function>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolNeed {
    /// The action word a session scope is written in (`read`, `create`, `update`, `call`).
    pub action: &'static str,
    /// The tier the action needs (the same table `spot-mcp` enforces: read T0, write T2,
    /// function T3).
    pub tier: Tier,
    /// The DocType the tool reaches, in the normalised form of the tool name, or `None` for a
    /// tenant-wide tool.
    pub doctype: Option<String>,
}

/// The need of tool `name`. An unknown shape is treated as the strongest (a function call), so a
/// new kind of tool is refused to a weak agent rather than allowed by default.
#[must_use]
pub fn classify(name: &str) -> ToolNeed {
    let reads = |rest: &str| ToolNeed {
        action: "read",
        tier: Tier::T0,
        doctype: Some(normalise(rest)),
    };
    if let Some(rest) = name.strip_prefix("search_") {
        return reads(rest);
    }
    if let Some(rest) = name.strip_prefix("get_") {
        return reads(rest);
    }
    if name == "query" || name == "job_status" {
        return ToolNeed {
            action: "read",
            tier: Tier::T0,
            doctype: None,
        };
    }
    if let Some(rest) = name
        .strip_prefix("create_")
        .and_then(|r| r.strip_suffix("_draft"))
    {
        return ToolNeed {
            action: "create",
            tier: Tier::T2,
            doctype: Some(normalise(rest)),
        };
    }
    if let Some(rest) = name.strip_prefix("update_") {
        return ToolNeed {
            action: "update",
            tier: Tier::T2,
            doctype: Some(normalise(rest)),
        };
    }
    ToolNeed {
        action: "call",
        tier: Tier::T3,
        doctype: None,
    }
}

/// The form a DocType name takes in a tool name: lower case, every other character `_`.
#[must_use]
pub fn normalise(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The tier an agent runs at. The stored registration is the only source. While the site policy
/// for unattended T1 and above (the M9 gate) is closed, every agent runs at T0: it may explain and
/// read, never change anything.
#[must_use]
pub fn effective_tier(reg: &Registration, policy_open: bool) -> Tier {
    if policy_open { reg.max_tier } else { Tier::T0 }
}

/// Why a call is refused, as a stable code. The sentence the caller sees is built from the code
/// alone, so it can never quote a stored value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl Refused {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.0
    }

    /// The caller-facing sentence.
    #[must_use]
    pub fn sentence(&self) -> String {
        format!("{}: this agent may not run this tool now", self.0)
    }
}

/// A grant with its scope's DocType names in the form tool names use, so a scope written
/// `Sales Invoice` matches the tool `get_sales_invoice`.
fn normalised(grant: &SessionGrant) -> SessionGrant {
    let mut copy = grant.clone();
    if let DocTypeScope::Named(names) = &mut copy.scope.doctypes {
        for name in names.iter_mut() {
            *name = normalise(name);
        }
    }
    copy
}

/// Decides one call by a registered agent.
///
/// First failing rule wins: a status other than active; a tool above the agent's effective tier;
/// for a support agent, no session that allows the action at `now`. An operations agent needs no
/// session. T1 and above are not reachable while `policy_open` is false.
///
/// # Errors
///
/// [`Refused`] with the stable code: `agent.suspended`, `agent.retired`, `agent.above_tier`,
/// `session.none`, or the support session's own `session.*` refusal code (the first grant's when
/// several refuse, so the caller learns why the newest attempt failed, never which grants exist).
pub fn decide(
    reg: &Registration,
    sessions: &[SessionGrant],
    tenant: &str,
    tool: &str,
    now: i64,
    policy_open: bool,
) -> Result<(), Refused> {
    match reg.status {
        Status::Active => {}
        Status::Suspended => return Err(Refused("agent.suspended".to_owned())),
        Status::Retired => return Err(Refused("agent.retired".to_owned())),
    }
    let need = classify(tool);
    if need.tier > effective_tier(reg, policy_open) {
        return Err(Refused("agent.above_tier".to_owned()));
    }
    if reg.domain != Domain::Support {
        return Ok(());
    }
    let request = ActionRequest {
        action: need.action.to_owned(),
        tier: need.tier,
        doctype: need.doctype,
        tenant: tenant.to_owned(),
    };
    let mut first: Option<Refusal> = None;
    for grant in sessions {
        match normalised(grant).check(&request, now) {
            Ok(()) => return Ok(()),
            Err(refusal) => {
                first.get_or_insert(refusal);
            }
        }
    }
    Err(Refused(first.map_or_else(
        || "session.none".to_owned(),
        |r| r.code().to_owned(),
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_support_session::{
        ActionScope, Approver, Scope, SessionGrant, SessionPolicy, SessionSpec, Subject,
        SubjectKind,
    };

    fn reg(domain: Domain, tier: Tier, status: Status) -> Registration {
        Registration {
            agent_id: "agent-1".into(),
            owner: "owner".into(),
            domain,
            max_tier: tier,
            deep: false,
            status,
        }
    }

    fn grant(tenant: &str, ceiling: Tier, doctypes: &[&str], start: i64, end: i64) -> SessionGrant {
        let spec = SessionSpec {
            id: "s1".into(),
            tenant: tenant.into(),
            subject: Subject::new(SubjectKind::Agent, "agent-1"),
            tier_ceiling: ceiling,
            scope: Scope {
                doctypes: DocTypeScope::Named(doctypes.iter().map(|d| (*d).to_owned()).collect()),
                actions: ActionScope::Any,
            },
            reason: "investigate a ticket".into(),
            requested_by: "support-lead".into(),
            starts_at: start,
            expires_at: end,
        };
        let policy = SessionPolicy::default();
        SessionGrant::request(&spec, &policy, start)
            .unwrap()
            .approve(&Approver::new("customer-owner", true), start, &policy)
            .unwrap()
            .0
    }

    #[test]
    fn tools_are_classified_by_name_and_the_unknown_is_the_strongest() {
        assert_eq!(classify("get_task").tier, Tier::T0);
        assert_eq!(
            classify("search_sales_invoice").doctype.as_deref(),
            Some("sales_invoice")
        );
        assert_eq!(classify("create_task_draft").tier, Tier::T2);
        assert_eq!(classify("update_task").action, "update");
        assert_eq!(classify("query").doctype, None);
        assert_eq!(classify("job_status").tier, Tier::T0);
        assert_eq!(classify("call_sample_reports_echo").tier, Tier::T3);
        assert_eq!(classify("something_new").tier, Tier::T3);
    }

    #[test]
    fn the_tier_comes_from_the_registration_and_is_t0_while_the_policy_is_closed() {
        let t2 = reg(Domain::Operations, Tier::T2, Status::Active);
        assert_eq!(effective_tier(&t2, false), Tier::T0);
        assert_eq!(effective_tier(&t2, true), Tier::T2);
    }

    #[test]
    fn a_t0_agent_reads_and_is_refused_a_create() {
        let r = reg(Domain::Operations, Tier::T0, Status::Active);
        assert!(decide(&r, &[], "acme", "get_task", 10, true).is_ok());
        assert_eq!(
            decide(&r, &[], "acme", "create_task_draft", 10, true)
                .unwrap_err()
                .code(),
            "agent.above_tier"
        );
    }

    #[test]
    fn a_registered_t1_or_t2_agent_is_still_refused_a_change_while_the_policy_is_closed() {
        let r = reg(Domain::Operations, Tier::T2, Status::Active);
        assert!(decide(&r, &[], "acme", "create_task_draft", 10, true).is_ok());
        assert_eq!(
            decide(&r, &[], "acme", "create_task_draft", 10, false)
                .unwrap_err()
                .code(),
            "agent.above_tier"
        );
        assert!(decide(&r, &[], "acme", "get_task", 10, false).is_ok());
    }

    #[test]
    fn a_suspended_or_retired_agent_is_refused_everything() {
        for (status, code) in [
            (Status::Suspended, "agent.suspended"),
            (Status::Retired, "agent.retired"),
        ] {
            let r = reg(Domain::Operations, Tier::T3, status);
            assert_eq!(
                decide(&r, &[], "acme", "get_task", 10, true)
                    .unwrap_err()
                    .code(),
                code
            );
        }
    }

    #[test]
    fn a_support_agent_needs_a_live_session_that_covers_the_tool() {
        let r = reg(Domain::Support, Tier::T2, Status::Active);
        assert_eq!(
            decide(&r, &[], "acme", "get_task", 150, true)
                .unwrap_err()
                .code(),
            "session.none"
        );
        let g = grant("acme", Tier::T0, &["Task"], 100, 1_000);
        assert!(decide(&r, std::slice::from_ref(&g), "acme", "get_task", 150, true).is_ok());
        // Out of scope DocType, above the session ceiling, other tenant, before and after the window.
        assert_eq!(
            decide(
                &r,
                std::slice::from_ref(&g),
                "acme",
                "get_project",
                150,
                true
            )
            .unwrap_err()
            .code(),
            "session.doctype_not_in_scope"
        );
        assert_eq!(
            decide(
                &r,
                std::slice::from_ref(&g),
                "acme",
                "create_task_draft",
                150,
                true
            )
            .unwrap_err()
            .code(),
            "session.above_ceiling"
        );
        assert_eq!(
            decide(&r, std::slice::from_ref(&g), "other", "get_task", 150, true)
                .unwrap_err()
                .code(),
            "session.wrong_tenant"
        );
        assert_eq!(
            decide(
                &r,
                std::slice::from_ref(&g),
                "acme",
                "get_task",
                1_000,
                true
            )
            .unwrap_err()
            .code(),
            "session.expired"
        );
        assert_eq!(
            decide(&r, std::slice::from_ref(&g), "acme", "get_task", 50, true)
                .unwrap_err()
                .code(),
            "session.not_yet_active"
        );
    }

    #[test]
    fn a_revoked_session_refuses_at_once() {
        let r = reg(Domain::Support, Tier::T2, Status::Active);
        let mut g = grant("acme", Tier::T0, &["Task"], 100, 1_000);
        g.revoke("customer-owner", "done", 200).unwrap();
        assert_eq!(
            decide(&r, std::slice::from_ref(&g), "acme", "get_task", 201, true)
                .unwrap_err()
                .code(),
            "session.revoked"
        );
    }

    #[test]
    fn the_refusal_sentence_quotes_only_the_code() {
        assert_eq!(
            Refused("session.expired".into()).sentence(),
            "session.expired: this agent may not run this tool now"
        );
    }
}
