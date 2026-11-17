//! Typed errors with stable `journal_access.*` codes. A message never carries a token, a key,
//! a grant id beside a credential, or journal content.

/// Why a grant operation was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// The caller is not a tenant administrator: only an administrator grants, renews, revokes
    /// or lists `journal_reader` grants, and a `journal_reader` cannot grant `journal_reader`.
    #[error("only a tenant administrator may grant, renew, revoke or list journal access")]
    NotAdministrator,
    /// The request itself is malformed (an empty or oversized id, a zero lifetime).
    #[error("the grant request is not valid: {0}")]
    GrantInvalid(&'static str),
    /// The lifetime asked for is longer than the limit for this kind of grant.
    #[error("the requested lifetime is longer than the {limit_days} day limit")]
    LifetimeExceeded {
        /// The limit that applied, in days.
        limit_days: u32,
    },
    /// The grantee already holds a live grant; renew it instead.
    #[error("the grantee already holds a live journal grant")]
    GrantExists,
    /// No grant has that id (or it is revoked and cannot be renewed).
    #[error("no such journal grant")]
    GrantUnknown,
    /// The grant store failed (never carries the database's own message).
    #[error("the journal grant store failed")]
    Store,
    /// The clock cannot be read, so no lifetime can be computed.
    #[error("the clock cannot be read")]
    ClockUnreadable,
    /// The settings are outside their bounds.
    #[error("the journal access settings are not valid: {0}")]
    SettingsInvalid(&'static str),
}

impl AccessError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAdministrator => "journal_access.not_administrator",
            Self::GrantInvalid(_) => "journal_access.grant_invalid",
            Self::LifetimeExceeded { .. } => "journal_access.lifetime_exceeded",
            Self::GrantExists => "journal_access.grant_exists",
            Self::GrantUnknown => "journal_access.grant_unknown",
            Self::Store => "journal_access.store_failed",
            Self::ClockUnreadable => "journal_access.clock_unreadable",
            Self::SettingsInvalid(_) => "journal_access.settings_invalid",
        }
    }
}

/// Why a read through the access surface did not serve bytes.
///
/// The wire form is deliberately coarse: every refusal of a caller without a usable grant is
/// one `403 permission.denied`, and only an expired grant adds `details.reason`
/// `grant_expired`, as the contract says. The finer reason goes to the audit record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// No usable grant. `expired` is true only when the caller's grant ran out.
    #[error("journal access denied")]
    Denied {
        /// Whether the wire answer carries `details.reason = grant_expired`.
        expired: bool,
    },
    /// The generation, segment or range is not in the signed manifest (or is not a canonical
    /// name at all).
    #[error("the generation or range is not a sealed journal segment")]
    NotFound,
    /// The principal is over its concurrency limit or its daily byte budget.
    #[error("the journal read rate limit is reached")]
    RateLimited {
        /// Seconds until a retry can succeed.
        retry_after_secs: u64,
    },
    /// The audit record could not be written, so nothing was served.
    #[error("the access audit record could not be written")]
    AuditUnavailable,
    /// The store, the manifest or a hash check failed.
    #[error("the journal could not be read")]
    Unavailable,
}

impl ReadError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied { .. } => "journal_access.denied",
            Self::NotFound => "journal_access.not_found",
            Self::RateLimited { .. } => "journal_access.rate_limited",
            Self::AuditUnavailable => "journal_access.audit_unavailable",
            Self::Unavailable => "journal_access.read_failed",
        }
    }
}
