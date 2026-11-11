//! The one error type of the crate, with stable, machine-readable `support_grants.*`
//! codes (blueprint section 4, in the style of `spot-support-session`'s `session.*`).

use spot_support_session::SessionError;

/// The stable codes this crate raises, one catalogue entry each
/// (`crates/framework/spot-errors/catalogue/support_grants.toml`).
pub mod codes {
    /// The action was inside the grant's scope, tier and window.
    pub const ALLOWED: &str = "support_grants.allowed";
    /// A customer administrator approved a grant.
    pub const APPROVED: &str = "support_grants.approved";
    /// A customer administrator extended a grant (a new approval).
    pub const EXTENDED: &str = "support_grants.extended";
    /// A support-side requester asked for a grant.
    pub const REQUESTED: &str = "support_grants.requested";
    /// A grant was revoked.
    pub const REVOKED: &str = "support_grants.revoked";
    /// A session started under a grant.
    pub const STARTED: &str = "support_grants.started";
    /// A session stopped.
    pub const STOPPED: &str = "support_grants.stopped";
    /// The grant's subject is not an active support agent allowed that tier.
    pub const AGENT_REFUSED: &str = "support_grants.agent_refused";
    /// The grant was already revoked; revocation is final.
    pub const ALREADY_REVOKED: &str = "support_grants.already_revoked";
    /// The customer-side audit chain could not record the event.
    pub const AUDIT_UNAVAILABLE: &str = "support_grants.audit_unavailable";
    /// A grant with that id already exists for the tenant.
    pub const DUPLICATE_GRANT: &str = "support_grants.duplicate_grant";
    /// The grant would run longer than the configured maximum.
    pub const DURATION_OVER_POLICY: &str = "support_grants.duration_over_policy";
    /// The extension was refused by the session policy.
    pub const EXTENSION_REFUSED: &str = "support_grants.extension_refused";
    /// The request is not in the shape a grant can have.
    pub const INVALID_REQUEST: &str = "support_grants.invalid_request";
    /// The approver is not a registered customer administrator of the tenant.
    pub const NOT_ADMIN: &str = "support_grants.not_admin";
    /// The grant is not running at the requested instant.
    pub const NOT_RUNNING: &str = "support_grants.not_running";
    /// The approver is the requester or the grant's subject.
    pub const SELF_APPROVAL: &str = "support_grants.self_approval";
    /// The underlying session model refused the request, approval, extension or revoke.
    pub const SESSION_REFUSED: &str = "support_grants.session_refused";
    /// The grant store could not complete the operation.
    pub const STORE_FAILED: &str = "support_grants.store_failed";
    /// The session is run by someone other than the grant's subject.
    pub const SUBJECT_MISMATCH: &str = "support_grants.subject_mismatch";
    /// The session names a support organisation the grant was not made to.
    pub const SUPPORT_ORG_MISMATCH: &str = "support_grants.support_org_mismatch";
    /// No grant with that id exists for the tenant.
    pub const UNKNOWN_GRANT: &str = "support_grants.unknown_grant";
    /// The action or grant is about another tenant.
    pub const WRONG_TENANT: &str = "support_grants.wrong_tenant";
}

/// A grant request, approval, extension, revocation or store operation that cannot be
/// completed.
///
/// The codes are stable; the messages are for people and never carry a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The request is not in the shape a grant can have: a blank id, tenant or support
    /// organisation, or a blank condition.
    #[error("the grant request is invalid: {0}")]
    InvalidRequest(String),

    /// The support agent is not enrolled, is not active, is not in the support domain, or
    /// its maximum tier is below the grant's ceiling.
    #[error("the support agent `{id}` cannot hold this grant: {detail}")]
    AgentRefused {
        /// The agent id the grant names.
        id: String,
        /// Why the agent cannot hold the grant.
        detail: String,
    },

    /// The approver is the requester or the grant's subject: nobody approves their own
    /// access.
    #[error("`{0}` cannot approve its own grant; a customer administrator must approve")]
    SelfApproval(String),

    /// The approver is not a registered administrator of the tenant.
    #[error("`{0}` is not a customer administrator of this tenant")]
    NotAdmin(String),

    /// The grant would run longer than the policy's configured maximum.
    #[error("the grant would run {asked} s, over the configured maximum of {max} s")]
    DurationOverPolicy {
        /// The asked duration, in seconds.
        asked: i64,
        /// The configured maximum, in seconds.
        max: i64,
    },

    /// The session model refused the request, approval, extension or revocation; the
    /// session's own stable `session.*` reason is preserved in the message.
    #[error("{0}")]
    Session(#[from] SessionError),

    /// The extension was refused by the session policy (expired, revoked, too many
    /// extensions, or over the total duration).
    #[error("the extension is refused: {0}")]
    ExtensionRefused(String),

    /// No grant with that id exists for the tenant.
    #[error("no grant `{id}` exists for tenant `{tenant}`")]
    UnknownGrant {
        /// The tenant the grant was looked up under.
        tenant: String,
        /// The grant id.
        id: String,
    },

    /// A grant with that id already exists for the tenant.
    #[error("a grant `{id}` already exists for tenant `{tenant}`")]
    DuplicateGrant {
        /// The tenant the grant belongs to.
        tenant: String,
        /// The grant id.
        id: String,
    },

    /// The grant was already revoked; revocation is final and a second revoke is refused.
    #[error("the grant `{0}` is already revoked; revocation is final")]
    AlreadyRevoked(String),

    /// The grant is not running at the requested instant: not yet started, expired or
    /// revoked.
    #[error("the grant `{id}` is not running at {now}: {state}")]
    NotRunning {
        /// The grant id.
        id: String,
        /// The instant asked about.
        now: i64,
        /// The observed state.
        state: String,
    },

    /// The action or grant names another tenant.
    #[error("the operation is about tenant `{asked}`, the grant covers `{grant}`")]
    WrongTenant {
        /// The tenant the operation names.
        asked: String,
        /// The tenant the grant covers.
        grant: String,
    },

    /// The customer-side audit chain could not record the event; the caller must fail
    /// closed.
    #[error("the customer-side audit chain could not record the event")]
    AuditUnavailable,

    /// The grant store could not complete the operation.
    #[error("the grant store failed: {0}")]
    StoreFailed(String),
}

impl GrantError {
    /// The stable `support_grants.*` code of this error.
    ///
    /// A session-model refusal keeps its own `session.*` code in the message; the code
    /// reported here is the crate's, so a caller can branch on the layer that refused.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => codes::INVALID_REQUEST,
            Self::AgentRefused { .. } => codes::AGENT_REFUSED,
            Self::SelfApproval(_) => codes::SELF_APPROVAL,
            Self::NotAdmin(_) => codes::NOT_ADMIN,
            Self::DurationOverPolicy { .. } => codes::DURATION_OVER_POLICY,
            Self::Session(_) => codes::SESSION_REFUSED,
            Self::ExtensionRefused(_) => codes::EXTENSION_REFUSED,
            Self::UnknownGrant { .. } => codes::UNKNOWN_GRANT,
            Self::DuplicateGrant { .. } => codes::DUPLICATE_GRANT,
            Self::AlreadyRevoked(_) => codes::ALREADY_REVOKED,
            Self::NotRunning { .. } => codes::NOT_RUNNING,
            Self::WrongTenant { .. } => codes::WRONG_TENANT,
            Self::AuditUnavailable => codes::AUDIT_UNAVAILABLE,
            Self::StoreFailed(_) => codes::STORE_FAILED,
        }
    }
}
