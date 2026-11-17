//! The invitation seam of `POST /api/v1/auth/invitations/accept` (`auth_invitation_accept`,
//! WP-IAM-12.T5; ADR-070 section 5, ADR-083).
//!
//! An administrator invites a person (`identity_user_create` with `invite: true`); the person
//! gets a token by mail and exchanges it **once** for a password. The token is the only
//! credential of the call, so the port is the only place that decides what it means:
//!
//! - the token is looked up by its **digest**; the token itself is never stored, logged or
//!   returned, and [`AcceptIn`] redacts it (and the password) in `Debug`;
//! - an unknown, expired, revoked or already used token is the one [`AcceptError::Invalid`]
//!   (`400 identity.invitation_invalid`), so the answer is not an oracle for which of them it
//!   was, and which tokens exist;
//! - a password that does not meet the tenant's policy is [`AcceptError::PasswordPolicy`]
//!   (`422 auth.password_policy`) and **does not use up the token**: the person tries again.
//!   The policy is judged only after the token is known to be valid, so the 422 tells nothing
//!   to someone who does not hold a valid token;
//! - a success sets the password, activates the invited user (no role is added: only the
//!   assignments the administrator made when inviting exist) and ends the invitation.
//!
//! [`MemoryInvitationAccept`] is the in-memory fake the route tests use, timed by an injected
//! clock. It judges a password by length only: a test double.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use spot_core::Clock;
use time::OffsetDateTime;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`InvitationAcceptPort`].
pub const KEY: PortKey = PortKey::of::<dyn InvitationAcceptPort>();

/// The call's input, already shaped by the route (lengths bounded, no unknown field).
#[derive(Clone, PartialEq, Eq)]
pub struct AcceptIn {
    /// The invitation token from the mail (a secret).
    pub token: String,
    /// The password the person chose (a secret).
    pub password: String,
    /// The name the person gave, when they changed it.
    pub full_name: Option<String>,
}

impl std::fmt::Debug for AcceptIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcceptIn")
            .field("token", &"<redacted>")
            .field("password", &"<redacted>")
            .field("full_name", &self.full_name.is_some())
            .finish()
    }
}

/// Why an invitation was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptError {
    /// The token is unknown, expired, revoked or already used: one answer for all of them.
    Invalid,
    /// The password does not meet the tenant's policy; the invitation is still open.
    PasswordPolicy,
    /// The store cannot answer now; nothing was changed.
    Unavailable,
}

/// Accepts invitations.
pub trait InvitationAcceptPort: Send + Sync + 'static {
    /// Exchanges the invitation token for the person's password on `ctx.site`, once.
    fn accept<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: AcceptIn,
    ) -> BoxFuture<'a, Result<(), AcceptError>>;
}

struct Record {
    site: String,
    expires_at: OffsetDateTime,
    revoked: bool,
    used: bool,
    password: Option<String>,
}

/// An in-memory [`InvitationAcceptPort`] for tests, with an injected clock.
pub struct MemoryInvitationAccept {
    clock: Arc<dyn Clock>,
    min_password: usize,
    records: Mutex<HashMap<[u8; 32], Record>>,
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

impl MemoryInvitationAccept {
    /// A port whose policy is "at least `min_password` characters".
    pub fn new(clock: Arc<dyn Clock>, min_password: usize) -> Self {
        MemoryInvitationAccept {
            clock,
            min_password,
            records: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Record>> {
        self.records.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records an invitation for `token` on `site`, open until `expires_at`.
    pub fn invite(&self, site: &str, token: &str, expires_at: OffsetDateTime) {
        self.lock().insert(
            digest(token),
            Record {
                site: site.to_owned(),
                expires_at,
                revoked: false,
                used: false,
                password: None,
            },
        );
    }

    /// Revokes the invitation of `token`.
    pub fn revoke(&self, token: &str) {
        if let Some(r) = self.lock().get_mut(&digest(token)) {
            r.revoked = true;
        }
    }

    /// The password the accepted invitation of `token` set, if it was accepted.
    pub fn password_of(&self, token: &str) -> Option<String> {
        self.lock()
            .get(&digest(token))
            .and_then(|r| r.password.clone())
    }
}

impl InvitationAcceptPort for MemoryInvitationAccept {
    fn accept<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: AcceptIn,
    ) -> BoxFuture<'a, Result<(), AcceptError>> {
        Box::pin(async move {
            let now = self.clock.now_utc();
            let mut records = self.lock();
            let Some(record) = records.get_mut(&digest(&input.token)) else {
                return Err(AcceptError::Invalid);
            };
            if record.site != ctx.site.as_str()
                || record.used
                || record.revoked
                || record.expires_at <= now
            {
                return Err(AcceptError::Invalid);
            }
            if input.password.chars().count() < self.min_password {
                return Err(AcceptError::PasswordPolicy);
            }
            record.used = true;
            record.password = Some(input.password);
            Ok(())
        })
    }
}
