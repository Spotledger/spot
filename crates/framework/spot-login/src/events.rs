//! Identity events and the append-only `Login Attempt` record.
//!
//! The service never writes an event itself: it hands them to an [`IdentityEvents`] sink. The
//! production sink writes the event to the outbox in the same transaction as the state change
//! (ADR-11); the tests collect them in memory. Nothing in an event or record carries a
//! credential, a token, a TOTP secret, a recovery code or a raw address: the source is already a
//! salted digest and the account is a sign-in name (ADR-070 §8.4).

use spot_core::SiteId;
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::error::LoginError;
use crate::ports::BoxFuture;

/// One `identity_*` event: its topic, the record it is about, and its validated payload.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityEvent {
    /// The identity topic, for example `identity_sign_in_succeeded`.
    pub topic: &'static str,
    /// The record the event is about; `None` for a sign-in attempt that matched no user (the
    /// topic's `user` field is then absent, so the audit never confirms a name).
    pub subject: Option<RecordId>,
    /// The topic-specific data object, in the events contract's schema.
    pub data: serde_json::Value,
}

/// One row of the append-only `Login Attempt` DocType.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginAttempt {
    /// The site the attempt reached.
    pub site: SiteId,
    /// The sign-in name that was presented; it need not belong to a user.
    pub account: String,
    /// The salted digest of the source address, rendered for storage; never the address.
    pub source_digest: String,
    /// The stable `login.*` result code.
    pub result_code: String,
    /// When the attempt happened.
    pub at: OffsetDateTime,
}

/// Where identity events and login attempts go.
pub trait IdentityEvents: Send + Sync + 'static {
    /// Delivers one identity event.
    fn emit<'a>(&'a self, event: IdentityEvent) -> BoxFuture<'a, Result<(), LoginError>>;

    /// Appends one login-attempt record.
    fn record_login_attempt<'a>(
        &'a self,
        attempt: LoginAttempt,
    ) -> BoxFuture<'a, Result<(), LoginError>>;
}
