//! The data of a session grant: subject, scope, spec, policy and the status values, all
//! serde-loadable so a grant crosses process boundaries and lands in the control plane
//! (WP-CTL-07) as a persisted, hand-readable record.

use serde::{Deserialize, Serialize};

use crate::tier::Tier;
/// What kind of principal may act inside a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// A support agent (WS13): an AI agent identity acting for the support organisation.
    Agent,
    /// A person (for example someone connecting through the MCP server on their own plan
    /// for a supervised support session).
    Person,
}

impl SubjectKind {
    /// The stable human text form, for summaries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Agent => "Support agent",
            SubjectKind::Person => "Support person",
        }
    }
}

/// The subject of a grant: the one who may act inside the session, an agent id or a
/// person id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Subject {
    /// Whether the subject is an agent or a person.
    pub kind: SubjectKind,
    /// The subject's id, as the control plane knows it.
    pub id: String,
}

impl Subject {
    /// A subject from a kind and an id.
    #[must_use]
    pub fn new(kind: SubjectKind, id: &str) -> Subject {
        Subject {
            kind,
            id: id.to_owned(),
        }
    }
}

/// Which DocTypes a session reaches.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocTypeScope {
    /// The named DocTypes only. The names are canonicalised at request time: trimmed,
    /// deduplicated and sorted, so equal scopes compare and summarise the same.
    Named(Vec<String>),
    /// Read-only across the whole tenant, every DocType, reads only. Only valid with a
    /// `T0` ceiling; the loader refuses anything wider.
    AllRead,
}

/// Which actions a session may run.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionScope {
    /// Any action, up to the grant's tier ceiling.
    Any,
    /// Only the named actions. The names are canonicalised at request time: trimmed,
    /// deduplicated and sorted, so equal scopes compare and summarise the same.
    Only(Vec<String>),
}

/// What a session is scoped to: its DocTypes and its actions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Scope {
    /// The DocTypes the session reaches.
    pub doctypes: DocTypeScope,
    /// The actions the session may run.
    pub actions: ActionScope,
}

/// A request for a session grant, before approval: what is asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SessionSpec {
    /// The grant's id, assigned by the caller (the control plane names its records).
    pub id: String,
    /// The tenant the session is about.
    pub tenant: String,
    /// Who may act inside the session.
    pub subject: Subject,
    /// The tier nothing inside the session may rise above.
    #[serde(with = "crate::tier::tier_snake")]
    pub tier_ceiling: Tier,
    /// What the session is scoped to.
    pub scope: Scope,
    /// Why the session is needed; blank reasons are refused.
    pub reason: String,
    /// Who asked for the session (a support-side person or agent).
    pub requested_by: String,
    /// The instant (unix seconds) the session may start acting, inclusive.
    pub starts_at: i64,
    /// The instant the session ends, exclusive: at `expires_at` the session is already
    /// expired (the half-open window `[starts_at, expires_at)`, see the crate docs).
    pub expires_at: i64,
}

/// The policy a tenant applies to session grants: the limits an approval may not pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SessionPolicy {
    /// The longest a session (including every extension, measured from the original
    /// start) may run. Default: 4 hours.
    pub max_duration_secs: i64,
    /// How many times a grant may be extended, each extension being a new approval.
    /// Default: 2.
    pub max_extensions: usize,
    /// Whether the approver must differ from the requester. Default: true (the
    /// segregation-of-duties default, ADR-070 section 8).
    pub require_distinct_approver: bool,
    /// The highest tier an approver who is not a tenant owner may approve. Default: `T1`.
    #[serde(with = "crate::tier::tier_snake")]
    pub max_tier_without_owner_approval: Tier,
}

impl Default for SessionPolicy {
    /// The shipped defaults: 4 hours, 2 extensions, a distinct approver required and at
    /// most `T1` without a tenant owner's approval.
    fn default() -> SessionPolicy {
        SessionPolicy {
            max_duration_secs: 4 * 3600,
            max_extensions: 2,
            require_distinct_approver: true,
            max_tier_without_owner_approval: Tier::T1,
        }
    }
}

/// The person on the customer side whose approval a grant or an extension needs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Approver {
    /// The approver's id.
    pub id: String,
    /// Whether the approver is a tenant owner, which lifts the
    /// [`SessionPolicy::max_tier_without_owner_approval`] limit.
    pub is_tenant_owner: bool,
}

impl Approver {
    /// An approver from an id and the owner flag.
    #[must_use]
    pub fn new(id: &str, is_tenant_owner: bool) -> Approver {
        Approver {
            id: id.to_owned(),
            is_tenant_owner,
        }
    }
}

/// A stored lifecycle state of a grant.
///
/// The crate itself sets only `Pending` (at request), `Active` (at approval) and `Revoked`
/// (at revocation). `Expired` is never set by the crate: expiry is **derived** from the
/// times ([`SessionGrant::status_at`], so nothing needs a background job), and `Expired`
/// exists in the wire format for callers that persist the derived state they observed.
/// The check and the lifecycle always derive expiry from the times; a stored `Expired`
/// flag is bookkeeping, never authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Requested, not yet approved.
    Pending,
    /// Approved (the stored state; whether the session is running is derived from the
    /// times).
    Active,
    /// Past its end time, derived from the times and possibly persisted by a caller.
    Expired,
    /// Revoked, final and immediate; a grant never reactivates.
    Revoked,
}

/// The liveness of a grant at one instant, derived from its times only.
///
/// This is the time view: revocation is read from the stored
/// [`SessionStatus::Revoked`], not from here, so this type carries no `Revoked` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedStatus {
    /// Before `starts_at`: the session may not act yet.
    NotYetActive,
    /// Inside `[starts_at, expires_at)`: the session runs.
    Active,
    /// At or after `expires_at`: the session is over (the boundary belongs to the expired
    /// side; see the crate docs).
    Expired,
}

/// One approved extension, kept in the grant's history (the brief: the history of
/// extensions stays in the grant).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Extension {
    /// Who approved the extension.
    pub approver: String,
    /// When the extension was approved (unix seconds).
    pub at: i64,
    /// The end instant the extension approved (unix seconds).
    pub extended_to: i64,
}
