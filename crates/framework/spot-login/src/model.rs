//! The domain types the sign-in service works on.
//!
//! These mirror the platform app's DocTypes (`user`, `session`, `api_key`) as the service reads
//! them through its ports. Credential material — a password hash, a TOTP secret, recovery-code
//! digests — is held in fields whose [`Debug`] is redacted and which are never cloned into logs.

use std::fmt;

use spot_auth::{PasswordHash, StoredDigest, TokenDigest, TotpSecret};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

/// The lifecycle state of a user (`user.state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserState {
    /// Invited but the invitation was never accepted: not signable.
    Invited,
    /// Enabled and signable.
    Active,
    /// Deactivated: not signable.
    Disabled,
    /// Anonymised: never signable again.
    Anonymised,
}

impl UserState {
    /// Parses the `user.state` select value.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "invited" => Some(UserState::Invited),
            "active" => Some(UserState::Active),
            "disabled" => Some(UserState::Disabled),
            "anonymised" => Some(UserState::Anonymised),
            _ => None,
        }
    }

    /// The stored select value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            UserState::Invited => "invited",
            UserState::Active => "active",
            UserState::Disabled => "disabled",
            UserState::Anonymised => "anonymised",
        }
    }

    /// Whether a user in this state may sign in at all.
    #[must_use]
    pub fn is_signable(self) -> bool {
        matches!(self, UserState::Active)
    }
}

/// A user as the sign-in service needs it, including credential material.
///
/// Not `Clone` on purpose: the credential fields are secret-class and must not be duplicated.
pub struct UserRecord {
    /// The stable record id.
    pub id: RecordId,
    /// The sign-in name (an e-mail address or a username).
    pub user_name: String,
    /// The e-mail address, when one is set.
    pub email: Option<String>,
    /// The display name.
    pub full_name: String,
    /// The lifecycle state.
    pub state: UserState,
    /// The stored Argon2id hash, when the user has a password.
    pub password_hash: Option<PasswordHash>,
    /// The enrolled TOTP secret, when the user has a second factor.
    pub totp_secret: Option<TotpSecret>,
    /// The stored single-use recovery-code digests.
    pub recovery_digests: Vec<StoredDigest>,
    /// The last accepted TOTP step, so a code cannot be replayed.
    pub totp_last_step: Option<u64>,
}

impl fmt::Debug for UserRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserRecord")
            .field("id", &self.id)
            .field("user_name", &self.user_name)
            .field("email", &self.email)
            .field("full_name", &self.full_name)
            .field("state", &self.state)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| "Secret(***)"),
            )
            .field(
                "totp_secret",
                &self.totp_secret.as_ref().map(|_| "Secret(***)"),
            )
            .field("recovery_digests", &self.recovery_digests.len())
            .field("totp_last_step", &self.totp_last_step)
            .finish()
    }
}

/// The lifecycle state of an API key (`api_key.state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// Usable.
    Active,
    /// Revoked by a user or administrator.
    Revoked,
    /// Past its expiry.
    Expired,
}

impl KeyState {
    /// Parses the `api_key.state` select value.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(KeyState::Active),
            "revoked" => Some(KeyState::Revoked),
            "expired" => Some(KeyState::Expired),
            _ => None,
        }
    }
}

/// An API key as the sign-in service reads it: the public id and the secret's digest only.
pub struct KeyRecord {
    /// The public key id (`spk_<key_id>_<secret>`).
    pub key_id: String,
    /// The digest of the secret part.
    pub digest: TokenDigest,
    /// The user the key belongs to.
    pub owner: RecordId,
    /// Its lifecycle state.
    pub state: KeyState,
    /// The instant it stops working, when one was set.
    pub expires_at: Option<OffsetDateTime>,
}

impl fmt::Debug for KeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRecord")
            .field("key_id", &self.key_id)
            .field("digest", &"Secret(***)")
            .field("owner", &self.owner)
            .field("state", &self.state)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// One row of a user's own session list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session's id.
    pub id: String,
    /// Whether this is the session making the request.
    pub current: bool,
    /// Whether the session is still usable.
    pub live: bool,
    /// Unix seconds the session started.
    pub created: i64,
    /// Unix seconds it was last seen.
    pub last_seen: i64,
    /// Unix seconds of its absolute expiry.
    pub expires_at: i64,
    /// The salted source digest, rendered for display (never the raw address).
    pub source: String,
    /// The device label, when one was recorded; a bounded display string, never a secret.
    pub device: Option<String>,
}
