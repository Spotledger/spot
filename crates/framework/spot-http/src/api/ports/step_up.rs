//! The step-up seam of `POST /api/v1/auth/step-up` (`auth_step_up`, WP-IAM-12.T7, contract
//! 0.18.x, ADR-070 section 3): a signed-in person proves who they are again so that a sensitive
//! operation (an API key, the single sign-on provider administration, governance, the data
//! policy, the security audit) is accepted for a short while.
//!
//! `spot-http` holds no credentials. It defines [`StepUpPort`], one operation in the shape the
//! router needs, and the host writes the adapter over the sign-in service. Everything that
//! decides is the port's: who the caller is (from `ctx.credential` on `ctx.site`, never from the
//! body), that it is a person of the staff or partner audience (never a consumer, never an API
//! key), that the proof is right (the password, and the TOTP code when the user is enrolled),
//! the throttle on wrong proofs, the marker that makes the session "recently stepped up" and the
//! audit of a success **and of a refusal**. The route passes no user id: there is no way to ask
//! for another user's step-up through this trait.
//!
//! [`IdentityStepUpPort`](super::identity::IdentityStepUpPort) is the other side: it only
//! *reads* whether the caller's step-up is recent, for the routes that need one. The host's
//! adapter of this port writes what that one reads.
//!
//! [`StepUpProof`] has a redacted `Debug`: a password or a code is never printed, and the
//! route never puts one in a log line, a metric label, an error or an idempotency record (the
//! operation takes no `Idempotency-Key` and the router never stores or replays its answer).

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`StepUpPort`].
pub const KEY: PortKey = PortKey::of::<dyn StepUpPort>();

/// Why a step-up was refused.
#[derive(Clone, Debug)]
pub enum StepUpError {
    /// A registry error, passed through with its code, status and details: `401
    /// auth.invalid_credentials` (exactly as the login answers it), `401 auth.totp_required`,
    /// `401 auth.totp_invalid`, `403 permission.denied`, `429 api.rate_limited` with its
    /// `retry_after` detail (the route turns it into `Retry-After`), `503 server.unavailable`.
    Api(ApiError),
    /// The site cannot serve this branch (for example a single sign-on session on a site whose
    /// single sign-on service offers no forced re-authentication): `501 service.not_configured`,
    /// which is not a registry code, so the route renders it itself.
    NotConfigured,
}

impl From<ApiError> for StepUpError {
    fn from(error: ApiError) -> Self {
        StepUpError::Api(error)
    }
}

/// What a port call answers.
pub type StepUpResult<T> = Result<T, StepUpError>;

/// What the caller proved. The route guarantees the shape: exactly one family, either a
/// `password` (with an optional `totp`) or `sso: true`; never both, never neither.
#[derive(Clone, PartialEq, Eq)]
pub struct StepUpProof {
    /// The caller's current password (1 to 1,024 bytes). `None` for the single sign-on family.
    pub password: Option<String>,
    /// The current second-factor code (1 to 16 bytes), when the caller sent one. Only with a
    /// password.
    pub totp: Option<String>,
    /// `true`: a single sign-on session asks to re-authenticate at its provider. Never with a
    /// password or a code.
    pub sso: bool,
}

impl std::fmt::Debug for StepUpProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only which parts are present; never a value.
        f.debug_struct("StepUpProof")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp", &self.totp.as_ref().map(|_| "<redacted>"))
            .field("sso", &self.sso)
            .finish()
    }
}

/// The answer of a step-up (the contract's `AuthStepUpResponse.data`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepUpOut {
    /// When the step-up lapses: a UTC RFC 3339 instant ending in `Z`. `None` when the answer is a
    /// redirect to the provider (the step-up is not done until the caller comes back).
    pub step_up_until: Option<String>,
    /// Where to send the person for a forced re-authentication at their single sign-on
    /// provider. `None` for a password step-up. Not a secret, but never logged.
    pub redirect_url: Option<String>,
}

/// The signed-in person's own step-up.
pub trait StepUpPort: Send + Sync {
    /// Re-checks the caller's own credential and, when it is right, records a step-up on the
    /// caller's own session. The caller is `ctx.credential`; nothing in `proof` names a user.
    fn step_up<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        proof: StepUpProof,
    ) -> BoxFuture<'a, StepUpResult<StepUpOut>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proof_debug_never_prints_a_secret() {
        let proof = StepUpProof {
            password: Some("hunter2-distinctive".to_owned()),
            totp: Some("987654".to_owned()),
            sso: false,
        };
        let shown = format!("{proof:?}");
        assert!(!shown.contains("hunter2-distinctive"), "{shown}");
        assert!(!shown.contains("987654"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn the_key_names_the_port_type() {
        assert_eq!(KEY, PortKey::of::<dyn StepUpPort>());
    }
}
