//! The check: may the subject run one action inside the session, at one instant?

use serde::{Deserialize, Serialize};

use crate::tier::Tier;

/// One action the subject asks to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ActionRequest {
    /// The action's name, as the action catalogue or MCP tool names it (`read`,
    /// `update`, `create`, `cancel`, `repair_workflow_state`, a tool name).
    pub action: String,
    /// The tier the action needs, decided by the caller's action catalogue (the same
    /// ladder the WP-AI-06.T1 authoriser checks).
    #[serde(with = "crate::tier::tier_snake")]
    pub tier: Tier,
    /// The DocType the action reaches, or `None` for a tenant-wide action (a diagnostic,
    /// a report over every DocType).
    pub doctype: Option<String>,
    /// The tenant the action is about.
    pub tenant: String,
}

/// Why a check refused: the **first** failing reason, in the fixed order.
///
/// Codes are stable `session.*` strings, the reason kinds in wire form; the order the
/// brief fixes is the order these variants are listed in. `to_string()` is the plain
/// sentence (it prints `code: message`); in a `SessionEvent::Refused` the code and the
/// reason travel separately.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The action is about another tenant.
    #[error(
        "session.wrong_tenant: the action is about tenant `{action}`, the grant covers `{grant}`"
    )]
    WrongTenant {
        /// The tier the action needed.
        tier: Tier,
        /// The tenant the action names.
        action: String,
        /// The tenant the grant covers.
        grant: String,
    },

    /// The session has not started yet (before `starts_at`).
    #[error("session.not_yet_active: the session starts at {starts_at} and now is {now}")]
    NotYetActive {
        /// The tier the action needed.
        tier: Tier,
        /// The session's start instant.
        starts_at: i64,
        /// The instant of the check.
        now: i64,
    },

    /// The session is over (at or after `expires_at`, the boundary belongs to the
    /// expired side).
    #[error("session.expired: the session expired at {expires_at} and now is {now}")]
    Expired {
        /// The tier the action needed.
        tier: Tier,
        /// The session's end instant.
        expires_at: i64,
        /// The instant of the check.
        now: i64,
    },

    /// The session was revoked; revocation is final and immediate.
    #[error("session.revoked: the session was revoked at {at} by {by}")]
    Revoked {
        /// When the revocation happened.
        at: i64,
        /// Who revoked the session.
        by: String,
    },

    /// The action's tier rises above the grant's ceiling.
    #[error(
        "session.above_ceiling: the action needs {action} and the session ceiling is {ceiling}"
    )]
    AboveCeiling {
        /// The tier the action needed.
        action: Tier,
        /// The grant's ceiling.
        ceiling: Tier,
    },

    /// The action is not in the session's scope (`Only` list).
    #[error(
        "session.action_not_in_scope: the action `{action}` is not in the session's action scope"
    )]
    ActionNotInScope {
        /// The action's name.
        action: String,
    },

    /// The action names a DocType outside the session's DocType scope.
    #[error(
        "session.doctype_not_in_scope: the DocType `{doctype}` is not in the session's DocType scope"
    )]
    DoctypeNotInScope {
        /// The DocType the action names.
        doctype: String,
    },

    /// A write inside an `AllRead` (read-only across the tenant) scope. Fires for any
    /// write-classed action name (`update`, `create`, `submit`, `cancel`, and friends),
    /// whatever the DocType scope would otherwise allow.
    #[error(
        "session.write_in_read_only_scope: the write `{action}` is refused in a read-only session"
    )]
    WriteInReadOnlyScope {
        /// The write's action name.
        action: String,
    },
}

const WRONG_TENANT: &str = "session.wrong_tenant";
const NOT_YET_ACTIVE: &str = "session.not_yet_active";
const EXPIRED: &str = "session.expired";
const REVOKED: &str = "session.revoked";
const ABOVE_CEILING: &str = "session.above_ceiling";
const ACTION_NOT_IN_SCOPE: &str = "session.action_not_in_scope";
const DOCTYPE_NOT_IN_SCOPE: &str = "session.doctype_not_in_scope";
const WRITE_IN_READ_ONLY_SCOPE: &str = "session.write_in_read_only_scope";

impl Refusal {
    /// The stable `session.*` code, in the fixed refusal order for the same reason kind.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::WrongTenant { .. } => WRONG_TENANT,
            Refusal::NotYetActive { .. } => NOT_YET_ACTIVE,
            Refusal::Expired { .. } => EXPIRED,
            Refusal::Revoked { .. } => REVOKED,
            Refusal::AboveCeiling { .. } => ABOVE_CEILING,
            Refusal::ActionNotInScope { .. } => ACTION_NOT_IN_SCOPE,
            Refusal::DoctypeNotInScope { .. } => DOCTYPE_NOT_IN_SCOPE,
            Refusal::WriteInReadOnlyScope { .. } => WRITE_IN_READ_ONLY_SCOPE,
        }
    }

    /// The tier the action needed, carried on the refusals that check it.
    #[must_use]
    pub fn tier(&self) -> Option<Tier> {
        match self {
            Refusal::WrongTenant { tier, .. }
            | Refusal::NotYetActive { tier, .. }
            | Refusal::Expired { tier, .. } => Some(*tier),
            Refusal::Revoked { .. }
            | Refusal::AboveCeiling { .. }
            | Refusal::ActionNotInScope { .. }
            | Refusal::DoctypeNotInScope { .. }
            | Refusal::WriteInReadOnlyScope { .. } => None,
        }
    }
}

const WRITE_ACTIONS: [&str; 13] = [
    "set", "update", "create", "save", "submit", "cancel", "delete", "remove", "patch", "insert",
    "amend", "assign", "close",
];

/// Whether `action` names a write, to refuse inside an
/// [`AllRead`](crate::model::DocTypeScope::AllRead) session.
///
/// Conservative by design: the action is recognised by its **last** segment over `.`,
/// `_` and `-` (`customer.update` is a write, `restore_customer_defaults` is not caught),
/// and any name the list does not catch must be listed in the session's action scope to
/// pass at all. The common lifecycle verbs of our own words are listed; a remedy with an
/// unusual name is listed in a named scope, never guessed.
pub(crate) fn is_write(action: &str) -> bool {
    let last = action.trim().rsplit(['.', '_', '-']).next().unwrap_or("");
    WRITE_ACTIONS.contains(&last)
}
