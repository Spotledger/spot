//! Second-factor self-service (WP-PLT-02.T9): enrol a TOTP factor, confirm it, remove it and
//! replace the recovery codes, for the signed-in person's own account.
//!
//! # Who may call
//!
//! A person with a **session** of the staff or partner audience. An API key is not a person (it
//! has no session and no second factor) and a consumer session is refused (ADR-083). The
//! caller's own user record is the only one these methods ever touch: there is no user id in
//! any argument. A session that is still restricted to choosing a password is stopped earlier by
//! the host's request gate.
//!
//! # Fresh proof
//!
//! Enrolment starts with the session alone (it changes nothing a sign-in depends on until
//! confirmed). Everything that **changes or removes** a second factor needs fresh proof in the
//! same request: confirming needs a valid code from the pending secret; removing or regenerating
//! needs the current password **and** a current code (or a recovery code, which is then
//! consumed). Enrolling on an account that already has a factor is refused (`identity.totp_state`,
//! 409), never replaced.
//!
//! # Replay and throttling
//!
//! A code is accepted once per time step: the step is stored with a compare-and-set
//! (`record_totp_step`). Every attempt that carries a password or a code is admitted through the
//! service's [`SignInAdmission`](crate::SignInAdmission) first, under the user's own name, so a
//! stolen session cannot guess codes faster than a sign-in could; a refusal is the sign-in's
//! rate-limited answer, and an admission that cannot count refuses (fail closed). Wrong codes and
//! wrong passwords answer the one `auth.totp_invalid` / `auth.invalid_credentials` of sign-in.
//!
//! # Effects
//!
//! Removing the factor, or replacing the recovery codes, ends the user's **other** sessions with
//! the reason `revoked_by_user`. Every success emits one identity event
//! (`identity_second_factor_enrolled`, `identity_second_factor_removed`,
//! `identity_recovery_codes_regenerated`); a failed proof is a failed attempt and is recorded as
//! one. No secret, code or digest is in an event, an error or a log line.

use spot_auth::{
    AuthError, Password, StoredDigest, TotpSecret, generate_recovery_codes, verify_recovery_code,
};
use spot_core::Audience;
use spot_http::api::{ApiError, Code, RequestCtx};
use spot_sessions::model::SessionEndReason;

use crate::admission::{Admission, SignInAttempt};
use crate::authenticator::{Authenticated, Via, user_key};
use crate::error::{LoginError, LoginErrorKind};
use crate::model::UserRecord;
use crate::service::LoginService;

/// The issuer shown in an authenticator app.
pub const TOTP_ISSUER: &str = "Spot";

/// What enrolment shows once.
pub struct Enrolment {
    /// The shared secret, base32. Shown once.
    pub secret: String,
    /// The `otpauth://` URI for an authenticator app. Shown once.
    pub provisioning_uri: String,
}

impl std::fmt::Debug for Enrolment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Enrolment(<redacted>)")
    }
}

fn state_error() -> ApiError {
    ApiError::new(
        Code::IdentityTotpState,
        "The second factor is not in the state this call needs.",
    )
}

fn invalid_code() -> ApiError {
    LoginError::new(LoginErrorKind::TotpInvalid, "").to_api_error()
}

impl LoginService {
    /// The caller as a person with a session of the staff or partner audience.
    async fn totp_caller(&self, ctx: &RequestCtx) -> Result<Authenticated, ApiError> {
        let principal = self.authenticator.authenticate(ctx).await?;
        if principal.session_id.is_none()
            || !matches!(principal.via, Via::Password)
            || !matches!(principal.audience, Audience::Staff | Audience::Partner)
        {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        Ok(principal)
    }

    async fn totp_user(
        &self,
        ctx: &RequestCtx,
        who: &Authenticated,
    ) -> Result<UserRecord, ApiError> {
        self.users
            .find_by_id(who.user_id)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?
            .ok_or_else(|| ApiError::from_code(Code::AuthInvalidCredentials))
    }

    /// Admits one proof attempt of the caller (a password or a code): the sign-in throttle,
    /// counted under the caller's own name. Fails closed.
    async fn totp_admit(&self, ctx: &RequestCtx, who: &Authenticated) -> Result<(), ApiError> {
        let tenant = ctx.site.to_string();
        let account = crate::service::normalise_identifier(&who.user_name);
        let peer = ctx.peer.map(|peer| peer.to_string());
        let connection = format!("no-peer/{account}");
        let answer = self
            .admission
            .admit(SignInAttempt {
                tenant: &tenant,
                account: &account,
                peer: peer.as_deref(),
                connection_id: Some(&connection),
            })
            .await;
        match answer {
            Ok(Admission::Allowed) => Ok(()),
            Ok(Admission::Denied { retry_after }) => Err(LoginError::new(
                LoginErrorKind::Throttled,
                format!(
                    "retry_after={}",
                    (retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0)).max(1)
                ),
            )
            .to_api_error_traced(ctx.request_id.as_str())),
            Err(_) => Err(LoginError::new(LoginErrorKind::AdmissionUnavailable, "").to_api_error()),
        }
    }

    /// Verifies a code against the user's active factor: a TOTP code (once per step; answers the
    /// step) or a recovery code (consumed; answers `None`). Counts nothing itself; the caller
    /// admitted the attempt.
    async fn totp_prove(
        &self,
        ctx: &RequestCtx,
        user: &mut UserRecord,
        code: &str,
    ) -> Result<Option<u64>, ApiError> {
        let Some(secret) = user.totp_secret.take() else {
            return Err(state_error());
        };
        match self
            .totp
            .verify(&secret, code, self.clock.as_ref(), user.totp_last_step)
        {
            Ok(step) => match self.users.record_totp_step(user.id, step).await {
                Ok(()) => {
                    user.totp_secret = Some(secret);
                    Ok(Some(step))
                }
                Err(error) if error.kind == LoginErrorKind::TotpReplayed => Err(invalid_code()),
                Err(error) => Err(error.to_api_error_traced(ctx.request_id.as_str())),
            },
            Err(AuthError::TotpReplayed) => Err(invalid_code()),
            Err(_) => match verify_recovery_code(code, &user.recovery_digests) {
                Some(index) => match self.users.consume_recovery_digest(user.id, index).await {
                    Ok(()) => {
                        user.totp_secret = Some(secret);
                        Ok(None)
                    }
                    Err(error)
                        if matches!(
                            error.kind,
                            LoginErrorKind::RecoveryCodeUsed | LoginErrorKind::RecoveryCodeInvalid
                        ) =>
                    {
                        Err(invalid_code())
                    }
                    Err(error) => Err(error.to_api_error_traced(ctx.request_id.as_str())),
                },
                None => Err(invalid_code()),
            },
        }
    }

    /// Verifies the caller's current password.
    fn totp_password(&self, user: &UserRecord, password: &str) -> Result<(), ApiError> {
        let given = Password::new(password);
        let ok = match &user.password_hash {
            Some(hash) => self.hasher.verify(&given, hash),
            None => {
                let _ = self.hasher.verify(&given, &self.dummy_hash);
                false
            }
        };
        if ok {
            Ok(())
        } else {
            Err(ApiError::from_code(Code::AuthInvalidCredentials))
        }
    }

    /// Starts enrolment: a fresh secret, stored sealed and **pending**, shown once.
    ///
    /// # Errors
    ///
    /// `identity.totp_state` when a second factor is already active; the shared authentication
    /// failures; `permission.denied` for an API key or a consumer session.
    pub async fn totp_enrol(&self, ctx: &RequestCtx) -> Result<Enrolment, ApiError> {
        let who = self.totp_caller(ctx).await?;
        let user = self.totp_user(ctx, &who).await?;
        if user.totp_secret.is_some() {
            return Err(state_error());
        }
        let secret = TotpSecret::generate();
        let uri = secret.provisioning_uri(
            &self.totp,
            TOTP_ISSUER,
            user.email.as_deref().unwrap_or(&user.user_name),
        );
        let base32 = secret.to_base32();
        self.users
            .begin_totp_enrolment(user.id, secret)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        Ok(Enrolment {
            secret: base32,
            provisioning_uri: uri,
        })
    }

    /// Confirms enrolment with the first code and returns the recovery codes, shown once.
    ///
    /// # Errors
    ///
    /// `identity.totp_state` when there is no enrolment in progress (or a factor is already
    /// active); `auth.totp_invalid` for a wrong code; the sign-in's rate-limited answer.
    pub async fn totp_confirm(
        &self,
        ctx: &RequestCtx,
        code: &str,
    ) -> Result<Vec<String>, ApiError> {
        let who = self.totp_caller(ctx).await?;
        let user = self.totp_user(ctx, &who).await?;
        if user.totp_secret.is_some() {
            return Err(state_error());
        }
        self.totp_admit(ctx, &who).await?;
        let Some(secret) = self
            .users
            .pending_totp(user.id)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?
        else {
            return Err(state_error());
        };
        let step = self
            .totp
            .verify(&secret, code, self.clock.as_ref(), None)
            .map_err(|_| invalid_code())?;
        let codes = generate_recovery_codes(self.recovery_codes);
        let displays: Vec<String> = codes.iter().map(|c| c.display()).collect();
        let digests: Vec<StoredDigest> = codes.iter().map(|c| c.digest()).collect();
        self.users
            .set_second_factor(user.id, Some(secret), digests)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        // The code that confirmed is spent: the same step cannot sign in or confirm again.
        match self.users.record_totp_step(user.id, step).await {
            Ok(()) => {}
            Err(error) if error.kind == LoginErrorKind::TotpReplayed => {}
            Err(error) => return Err(error.to_api_error_traced(ctx.request_id.as_str())),
        }
        self.emit(
            "identity_second_factor_enrolled",
            Some(user.id),
            serde_json::json!({
                "user": user.id.to_string(),
                "factor": "totp",
                "factor_id": user.id.to_string(),
            }),
        )
        .await?;
        Ok(displays)
    }

    /// Removes the second factor. Needs the password and a current code (or a recovery code).
    ///
    /// # Errors
    ///
    /// `identity.totp_state` when no factor is active; `auth.invalid_credentials` for a wrong
    /// password; `auth.totp_invalid` for a wrong code; the sign-in's rate-limited answer.
    pub async fn totp_disable(
        &self,
        ctx: &RequestCtx,
        password: &str,
        code: &str,
    ) -> Result<(), ApiError> {
        let who = self.totp_caller(ctx).await?;
        let mut user = self.totp_user(ctx, &who).await?;
        if user.totp_secret.is_none() {
            return Err(state_error());
        }
        self.totp_admit(ctx, &who).await?;
        self.totp_password(&user, password)?;
        let _ = self.totp_prove(ctx, &mut user, code).await?;
        self.users
            .set_second_factor(user.id, None, Vec::new())
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        self.totp_end_other_sessions(ctx, &who).await?;
        self.emit(
            "identity_second_factor_removed",
            Some(user.id),
            serde_json::json!({
                "user": user.id.to_string(),
                "factor": "totp",
                "factor_id": user.id.to_string(),
                "by": "user",
            }),
        )
        .await
    }

    /// Replaces the recovery codes: the old batch stops working, the new codes are shown once.
    ///
    /// # Errors
    ///
    /// As [`LoginService::totp_disable`].
    pub async fn totp_regenerate_recovery(
        &self,
        ctx: &RequestCtx,
        password: &str,
        code: &str,
    ) -> Result<Vec<String>, ApiError> {
        let who = self.totp_caller(ctx).await?;
        let mut user = self.totp_user(ctx, &who).await?;
        if user.totp_secret.is_none() {
            return Err(state_error());
        }
        self.totp_admit(ctx, &who).await?;
        self.totp_password(&user, password)?;
        let proven = self.totp_prove(ctx, &mut user, code).await?;
        let Some(secret) = user.totp_secret.take() else {
            return Err(state_error());
        };
        let codes = generate_recovery_codes(self.recovery_codes);
        let displays: Vec<String> = codes.iter().map(|c| c.display()).collect();
        let digests: Vec<StoredDigest> = codes.iter().map(|c| c.digest()).collect();
        self.users
            .set_second_factor(user.id, Some(secret), digests)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        // `set_second_factor` restarts the replay counter; the code just used is spent again.
        if let Some(step) = proven {
            let _ = self.users.record_totp_step(user.id, step).await;
        }
        self.totp_end_other_sessions(ctx, &who).await?;
        self.emit(
            "identity_recovery_codes_regenerated",
            Some(user.id),
            serde_json::json!({
                "user": user.id.to_string(),
                "count": displays.len(),
                "by": "user",
            }),
        )
        .await?;
        Ok(displays)
    }

    async fn totp_end_other_sessions(
        &self,
        ctx: &RequestCtx,
        who: &Authenticated,
    ) -> Result<(), ApiError> {
        let key = user_key(who.user_id)
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        self.sessions
            .revoke_all_for_user(
                &key,
                who.session_id.as_deref(),
                SessionEndReason::Other("revoked_by_user".to_owned()),
            )
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;
        self.authenticator.invalidate_all();
        Ok(())
    }
}
