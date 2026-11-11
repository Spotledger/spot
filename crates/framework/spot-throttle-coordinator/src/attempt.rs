//! What the caller says about one attempt, and the answer.

use std::time::Duration;

use crate::error::ThrottleError;

/// The longest account name, address, e-mail or phone the throttle accepts, in bytes. A longer
/// value is refused before it is hashed: the throttle's own cost must not be attackable.
pub const MAX_VALUE_BYTES: usize = 1024;

/// The longest tenant id, in bytes (it is part of every bucket key, which a coordinator
/// limits to 200 bytes).
pub const MAX_TENANT_BYTES: usize = 64;

/// Whether `tenant` is a plain id: 1 to [`MAX_TENANT_BYTES`] printable ASCII bytes without a
/// colon (a colon would make `thr:{tenant}:{dimension}:{digest}` ambiguous).
///
/// It is the rule the attempt and the salt derivation share, so a tenant that can be counted can
/// also be given a salt, and neither accepts something the other refuses.
pub(crate) fn is_plain_tenant(tenant: &str) -> bool {
    !tenant.is_empty()
        && tenant.len() <= MAX_TENANT_BYTES
        && tenant.bytes().all(|b| b.is_ascii_graphic() && b != b':')
}

/// Which front-door action is being counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A password (or other credential) check. Needs an account name.
    SignIn,
    /// A new registration. Account and destination are both optional.
    Register,
    /// A request to send a one-time code. Needs a destination.
    CodeRequest,
    /// An account recovery. Needs an account name or a destination.
    Recovery,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::Register => "register",
            Self::CodeRequest => "code_request",
            Self::Recovery => "recovery",
        }
    }
}

/// Where an attempt came from.
///
/// The address is whatever the edge trusts as the peer (the caller canonicalises it; an IPv6
/// caller normally passes the /64). An **empty address** is never one shared bucket:
/// it is bucketed per `connection_id` when the caller has one, and refused when not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceAddr<'a> {
    /// The peer address; empty (or blank) when the edge could not tell.
    pub address: &'a str,
    /// An id of the connection, used only when `address` is empty.
    pub connection_id: Option<&'a str>,
}

impl<'a> SourceAddr<'a> {
    /// A source with a peer address.
    pub fn new(address: &'a str) -> Self {
        Self {
            address,
            connection_id: None,
        }
    }

    /// A source whose address is unknown, identified by its connection only.
    pub fn connection(connection_id: &'a str) -> Self {
        Self {
            address: "",
            connection_id: Some(connection_id),
        }
    }
}

/// One attempt to count.
#[derive(Clone, Copy, Debug)]
pub struct Attempt<'a> {
    /// What is being attempted.
    pub kind: Kind,
    /// The account name as typed, **known or not** (an unknown name must be passed and is
    /// counted exactly like a real one).
    pub account: Option<&'a str>,
    /// Where it came from.
    pub source: SourceAddr<'a>,
    /// The e-mail address or phone number a code would be sent to.
    pub destination: Option<&'a str>,
    /// The tenant (a plain id: 1 to 64 printable ASCII bytes without a colon).
    pub tenant: &'a str,
}

/// The throttle's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead (and the attempt has been counted).
    Allow,
    /// Refuse; the same attempt can succeed after `retry_after`.
    Deny {
        /// How long until the denying bucket has refilled enough.
        retry_after: Duration,
    },
}

impl Verdict {
    /// `Ok` for `Allow`, `throttle_coordinator.rate_limited` for `Deny`.
    pub fn require_allowed(self) -> Result<(), ThrottleError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny { retry_after } => Err(ThrottleError::RateLimited { retry_after }),
        }
    }

    /// Whether the attempt may proceed.
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

impl Attempt<'_> {
    /// Refuses a malformed attempt before anything is counted.
    pub(crate) fn validate(&self) -> Result<(), ThrottleError> {
        let bad = |reason| Err(ThrottleError::BadAttempt { reason });
        if !is_plain_tenant(self.tenant) {
            return bad("the tenant must be 1 to 64 printable ASCII bytes without a colon");
        }
        for value in [self.account, self.destination].into_iter().flatten() {
            if value.trim().is_empty() {
                return bad("an account name or destination must not be empty");
            }
            if value.len() > MAX_VALUE_BYTES {
                return bad("an account name or destination is too long");
            }
        }
        if self.source.address.len() > MAX_VALUE_BYTES
            || self
                .source
                .connection_id
                .is_some_and(|id| id.len() > MAX_VALUE_BYTES)
        {
            return bad("a source address or connection id is too long");
        }
        if self.source.address.trim().is_empty()
            && self
                .source
                .connection_id
                .is_none_or(|id| id.trim().is_empty())
        {
            return bad("the source has neither an address nor a connection id");
        }
        let needs = match self.kind {
            Kind::SignIn => self.account.is_some(),
            Kind::Register => true,
            Kind::CodeRequest => self.destination.is_some(),
            Kind::Recovery => self.account.is_some() || self.destination.is_some(),
        };
        if !needs {
            return bad(
                "this kind of attempt is missing the account name or destination it counts",
            );
        }
        Ok(())
    }
}
