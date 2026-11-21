//! The second-factor seam of `POST /api/v1/auth/totp/{enrol,confirm,disable}` and
//! `POST /api/v1/auth/recovery-codes/regenerate` (`auth_totp_enrol`, `auth_totp_confirm`,
//! `auth_totp_disable`, `auth_recovery_codes_regenerate`, WP-PLT-02.T9, contract 0.18.0).
//!
//! `spot-http` holds no credentials. It defines [`TotpPort`], the four operations in the shapes
//! the router needs, and the host writes the adapter over the sign-in service. Everything that
//! decides is the port's: who the caller is (from `ctx.credential` on `ctx.site`), that it is a
//! person with a session of an audience that may enrol, the proof each change needs (the current
//! password and a code), the throttling of wrong proofs, the effect on other sessions and the
//! audit of a success **and of a refusal**. The routes pass no user id: there is no way to ask
//! for another user's factor through this trait.
//!
//! The values that leave are [`EnrolOut`] and [`CodesOut`], shown once. Their `Debug` is
//! redacted, and the group never puts them in an idempotency record (these operations take no
//! `Idempotency-Key` and are not replayable).

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`TotpPort`].
pub const KEY: PortKey = PortKey::of::<dyn TotpPort>();

/// What a port call answers.
pub type TotpResult<T> = Result<T, ApiError>;

/// The provisioning secret and URI of a started enrolment. Shown once.
pub struct EnrolOut {
    /// The shared secret, base32.
    pub secret: String,
    /// The `otpauth://` URI an authenticator app reads.
    pub provisioning_uri: String,
}

impl std::fmt::Debug for EnrolOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnrolOut(<redacted>)")
    }
}

/// A set of recovery codes. Shown once.
pub struct CodesOut {
    /// The codes, as the user types them.
    pub recovery_codes: Vec<String>,
}

impl std::fmt::Debug for CodesOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CodesOut(<redacted>)")
    }
}

/// The signed-in person's own second factor.
pub trait TotpPort: Send + Sync {
    /// Starts enrolment: the secret is stored pending (not a factor yet) and shown once. A user
    /// who already has a factor is refused (`identity.totp_state`), not replaced.
    fn enrol<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, TotpResult<EnrolOut>>;

    /// Confirms the pending enrolment with its first code; answers the recovery codes, once.
    fn confirm<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>>;

    /// Removes the factor. Needs the current password and a current (or recovery) code.
    fn disable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<()>>;

    /// Replaces the recovery codes. Needs the current password and a current (or recovery) code.
    fn regenerate<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>>;
}
