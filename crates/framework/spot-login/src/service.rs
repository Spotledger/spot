//! [`LoginService`]: the real `AuthService` for the native API.
//!
//! `login` runs the throttle check *before* any hash work, verifies the password in constant time
//! even for an unknown user (against a fixed dummy hash, so timing and error reveal nothing),
//! requires and verifies a second factor when the user has one, and starts a session. The
//! throttle counts the attempt against the account and against the request's source:
//! [`AuthService::login`] takes that source from [`RequestCtx::peer`], the address the HTTP edge
//! already attributes the request to (the connection, or the forwarded client when the connection
//! comes from a configured trusted proxy), so one caller guessing across many accounts is backed
//! off as a source while a failure on one account never slows another source or another account.
//! `logout` ends the current session idempotently and `session` reports it. The extra operations of
//! native API v0.3.0 — change password, list my sessions, end one session — are inherent methods,
//! and [`LoginService::authenticate`] is the one call every other service makes to turn a request
//! credential into a user.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_auth::{
    AuthError, HashParams, Password, PasswordPolicy, TotpConfig, verify_recovery_code,
};
use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_http::api::{
    ApiError, AuthService, Code, LoginBody, RequestCtx, Session, SessionInfo, UserInfo,
};
use spot_sessions::model::SessionEndReason;
use spot_sessions::throttle::{AccountKey, FailureRecord, LoginThrottle};
use spot_sessions::{
    ClientInfo, SaltKey, SessionManager, SessionPolicy, SessionStore, ThrottleConfig,
    ThrottleDecision, ThrottleError, ThrottleStore,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::authenticator::{Authenticated, Authenticator, user_key};
use crate::error::{LoginError, LoginErrorKind, invalid_credentials};
use crate::events::{IdentityEvent, IdentityEvents, LoginAttempt};
use crate::model::{SessionSummary, UserState};
use crate::ports::{KeyStore, PasswordHasher, UserDirectory};

/// The `Login Attempt` result code for a successful sign-in.
pub const RESULT_OK: &str = "ok";

/// The configuration a [`LoginService`] runs with.
pub struct LoginConfig {
    /// Session idle/absolute policy.
    pub session_policy: SessionPolicy,
    /// Throttle thresholds and backoff.
    pub throttle: ThrottleConfig,
    /// TOTP algorithm, digits and skew.
    pub totp: TotpConfig,
    /// The tenant password policy enforced by `change_password`.
    pub password_policy: PasswordPolicy,
    /// How many recovery codes a reset issues.
    pub recovery_codes: usize,
    /// The authenticator's session-cache TTL (the cross-node staleness bound).
    pub cache_ttl: Duration,
    /// Argon2id cost parameters the provided hasher uses.
    pub hash_params: HashParams,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            session_policy: SessionPolicy::DEFAULT,
            throttle: ThrottleConfig::DEFAULT,
            totp: TotpConfig {
                algorithm: spot_auth::TotpAlgorithm::Sha1,
                digits: spot_auth::TotpDigits::Six,
                step_seconds: spot_auth::totp::DEFAULT_STEP_SECONDS,
                skew_steps: spot_auth::totp::DEFAULT_SKEW_STEPS,
            },
            password_policy: PasswordPolicy::default(),
            recovery_codes: spot_auth::recovery::DEFAULT_RECOVERY_CODE_COUNT,
            cache_ttl: Duration::from_secs(5),
            hash_params: HashParams::default(),
        }
    }
}

impl LoginConfig {
    /// Cheap parameters for tests: a fast hash and a one-step skew window.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            hash_params: HashParams::for_tests(),
            ..Self::default()
        }
    }
}

/// A `ThrottleStore` shared behind an `Arc`, forwarding every call.
pub(crate) struct SharedThrottleStore(Arc<dyn ThrottleStore>);

impl ThrottleStore for SharedThrottleStore {
    fn get<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<FailureRecord, ThrottleError>> + Send + 'a>> {
        self.0.get(account, key)
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), ThrottleError>> + Send + 'a>> {
        self.0.put(account, key, record)
    }

    fn clear<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), ThrottleError>> + Send + 'a>> {
        self.0.clear(account, key)
    }
}

/// Where a sign-in attempt came from, as the HTTP edge knows it.
///
/// [`RequestCtx::peer`] carries the address the request is attributed to — the connection, or the
/// forwarded client when the connection comes from a configured trusted proxy — so the native
/// [`AuthService::login`] passes [`LoginSource::peer`] and the API is throttled per source.
/// [`LoginSource::unknown`] remains for a caller with no address at all (an in-process call, or an
/// edge that could not resolve one), and it is bucketed per account. This crate never parses
/// headers: the address is whatever the edge resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginSource {
    peer: Option<String>,
}

impl LoginSource {
    /// A known peer address (an empty or blank text counts as unknown).
    #[must_use]
    pub fn peer(address: impl Into<String>) -> Self {
        let address = address.into();
        if address.trim().is_empty() {
            Self::unknown()
        } else {
            Self {
                peer: Some(address),
            }
        }
    }

    /// No peer address is available.
    #[must_use]
    pub fn unknown() -> Self {
        Self { peer: None }
    }

    /// The peer address, when one is known.
    #[must_use]
    pub fn address(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    /// The source text the throttle keys on.
    ///
    /// With no peer there is no honest per-source bucket, and one shared bucket for every
    /// caller would let three failures from anyone back off every sign-in. An unknown
    /// peer is therefore bucketed per account: the source counter then mirrors that account's
    /// own counter and no failure on one account can slow another.
    fn throttle_key(&self, identifier: &str) -> String {
        match &self.peer {
            Some(address) => address.clone(),
            None => format!("no-peer/{identifier}"),
        }
    }
}

/// The sign-in service. Build once per site and share it.
pub struct LoginService {
    pub(crate) sessions: SessionManager<Arc<dyn SessionStore>>,
    pub(crate) throttle: LoginThrottle<SharedThrottleStore>,
    pub(crate) users: Arc<dyn UserDirectory>,
    pub(crate) keys: Arc<dyn KeyStore>,
    pub(crate) events: Arc<dyn IdentityEvents>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) hasher: Arc<dyn PasswordHasher>,
    pub(crate) authenticator: Arc<Authenticator>,
    pub(crate) salt: Arc<SaltKey>,
    pub(crate) totp: TotpConfig,
    pub(crate) password_policy: PasswordPolicy,
    pub(crate) recovery_codes: usize,
    pub(crate) dummy_hash: spot_auth::PasswordHash,
}

impl LoginService {
    /// Builds the service over the shared session store and the user, key and event ports.
    ///
    /// # Errors
    ///
    /// [`LoginErrorKind::StoreFailed`] if the throttle configuration is invalid or the dummy
    /// hash cannot be built.
    pub fn new(
        config: LoginConfig,
        store: Arc<dyn SessionStore>,
        throttle_store: Arc<dyn ThrottleStore>,
        users: Arc<dyn UserDirectory>,
        keys: Arc<dyn KeyStore>,
        events: Arc<dyn IdentityEvents>,
        clock: Arc<dyn Clock>,
        hasher: Arc<dyn PasswordHasher>,
    ) -> Result<Self, LoginError> {
        let salt = Arc::new(SaltKey::generate());
        let sessions = SessionManager::new(store.clone(), config.session_policy, salt.clone());
        let throttle = LoginThrottle::new(
            SharedThrottleStore(throttle_store),
            config.throttle,
            salt.clone(),
        )
        .map_err(|error| LoginError::new(LoginErrorKind::StoreFailed, error.to_string()))?;
        // A fixed hash that no real password verifies against: the work an unknown-user attempt
        // does, so it cannot be told from a wrong-password attempt.
        let dummy_hash = hasher.hash(&Password::new("spot-login/never-a-real-password"));
        let authenticator = Arc::new(Authenticator::new(
            store,
            config.session_policy,
            salt.clone(),
            users.clone(),
            keys.clone(),
            clock.clone(),
            config.cache_ttl,
        ));
        Ok(Self {
            sessions,
            throttle,
            users,
            keys,
            events,
            clock,
            hasher,
            authenticator,
            salt,
            totp: config.totp,
            password_policy: config.password_policy,
            recovery_codes: config.recovery_codes,
            dummy_hash,
        })
    }

    /// Authenticates a request credential: the call every secured service makes.
    ///
    /// # Errors
    ///
    /// `auth.required` when no credential was sent; otherwise one shared invalid-credentials
    /// answer, so the caller cannot tell an unknown token from a revoked, expired or foreign one.
    pub async fn authenticate(&self, ctx: &RequestCtx) -> Result<Authenticated, ApiError> {
        self.authenticator.authenticate(ctx).await
    }

    /// The authenticator's session-cache TTL (the documented cross-node staleness bound).
    #[must_use]
    pub fn cache_ttl(&self) -> Duration {
        self.authenticator.cache_ttl()
    }

    /// Signs a user in, taking the raw source address for per-source throttling. An empty
    /// source is an unknown peer (see [`LoginSource`]).
    ///
    /// # Errors
    ///
    /// The contract's `auth.*` failures; see the module docs.
    pub async fn login_with_source(
        &self,
        ctx: &RequestCtx,
        body: &LoginBody,
        source: &str,
    ) -> Result<Session, ApiError> {
        self.login_from(ctx, body, &LoginSource::peer(source)).await
    }

    /// Signs a user in from `from`. The throttle keys on (account, source): failures from one
    /// source back that source off, and a source that is not known backs off only the account it
    /// failed on, so no single caller can put every sign-in into a back-off.
    ///
    /// The native [`AuthService::login`] takes `from` from [`RequestCtx::peer`]; callers that hold
    /// the address in another form use this method or [`LoginService::login_with_source`].
    ///
    /// # Errors
    ///
    /// The contract's `auth.*` failures; see the module docs.
    pub async fn login_from(
        &self,
        ctx: &RequestCtx,
        body: &LoginBody,
        from: &LoginSource,
    ) -> Result<Session, ApiError> {
        let now = self.clock.now_utc().unix_timestamp();
        let identifier = normalise_identifier(&body.user);
        let session_source = from.address().unwrap_or("");
        let throttle_source = from.throttle_key(&identifier);
        let source = throttle_source.as_str();
        if identifier.is_empty() {
            let account = AccountKey::new("(empty)").map_err(|reason| {
                LoginError::new(LoginErrorKind::StoreFailed, reason)
                    .to_api_error_traced(ctx.request_id.as_str())
            })?;
            self.fail(
                ctx,
                &account,
                &identifier,
                source,
                None,
                LoginErrorKind::InvalidIdentifier,
                now,
            )
            .await?;
            return Err(invalid_credentials());
        }

        let account = AccountKey::new(identifier.clone()).map_err(|reason| {
            LoginError::new(LoginErrorKind::StoreFailed, reason)
                .to_api_error_traced(ctx.request_id.as_str())
        })?;

        match self
            .throttle
            .check(&account, source, now)
            .await
            .map_err(|error| LoginError::from(error).to_api_error_traced(ctx.request_id.as_str()))?
        {
            ThrottleDecision::Allow => {}
            ThrottleDecision::Delay { delay, .. } => {
                self.record_attempt(
                    ctx,
                    &identifier,
                    source,
                    LoginErrorKind::Throttled.code(),
                    now,
                )
                .await?;
                let error = LoginError::new(
                    LoginErrorKind::Throttled,
                    format!("retry_after={}", delay.as_secs()),
                );
                return Err(error.to_api_error_traced(ctx.request_id.as_str()));
            }
            ThrottleDecision::Locked { until } => {
                self.record_attempt(
                    ctx,
                    &identifier,
                    source,
                    LoginErrorKind::AccountLocked.code(),
                    now,
                )
                .await?;
                let wait = u64::try_from((until - now).max(0)).unwrap_or(0);
                let error =
                    LoginError::new(LoginErrorKind::AccountLocked, format!("retry_after={wait}"));
                return Err(error.to_api_error_traced(ctx.request_id.as_str()));
            }
        }

        let password = Password::new(&body.password);
        let found = self
            .users
            .find_by_identifier(&identifier)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        let Some(mut user) = found else {
            // The whole point: an unknown user does exactly the hash work a known one does.
            let _ = self.hasher.verify(&password, &self.dummy_hash);
            self.fail(
                ctx,
                &account,
                &identifier,
                source,
                None,
                LoginErrorKind::InvalidCredentials,
                now,
            )
            .await?;
            return Err(invalid_credentials());
        };

        let mut verified = match &user.password_hash {
            Some(hash) => self.hasher.verify(&password, hash),
            None => {
                let _ = self.hasher.verify(&password, &self.dummy_hash);
                false
            }
        };
        let mut refusal = LoginErrorKind::InvalidCredentials;
        if !verified {
            // First sign-in: the presented text may be the user's one-time initial secret
            // (`spot new-site`). Spent in one transaction; an unknown, expired, foreign or
            // already-used one is an ordinary wrong password to the caller (the audit keeps
            // the true reason).
            //
            // The user's state is checked first: a disabled, anonymised or invited
            // account is refused below without spending the secret, so it still works once
            // the account may sign in.
            if user.state.is_signable()
                && let Ok(token) = spot_auth::OpaqueToken::parse(&body.password)
            {
                match self.keys.consume_initial_secret(user.id, &token, now).await {
                    Ok(()) => verified = true,
                    Err(error) if error.kind == LoginErrorKind::InitialSecretUsed => {
                        refusal = LoginErrorKind::InitialSecretUsed;
                    }
                    Err(error) if error.kind == LoginErrorKind::InitialSecretInvalid => {}
                    Err(error) => return Err(error.to_api_error_traced(ctx.request_id.as_str())),
                }
            }
        }
        if !verified {
            self.fail(
                ctx,
                &account,
                &identifier,
                source,
                Some(user.id),
                refusal,
                now,
            )
            .await?;
            return Err(invalid_credentials());
        }

        if !user.state.is_signable() {
            let kind = match user.state {
                UserState::Disabled => LoginErrorKind::AccountDisabled,
                UserState::Invited => LoginErrorKind::AccountInvited,
                UserState::Anonymised => LoginErrorKind::AccountAnonymised,
                UserState::Active => LoginErrorKind::InvalidCredentials,
            };
            self.fail(ctx, &account, &identifier, source, Some(user.id), kind, now)
                .await?;
            return Err(invalid_credentials());
        }

        match self.check_second_factor(ctx, &mut user, body).await? {
            Ok(()) => {}
            Err(kind) if kind == LoginErrorKind::TotpRequired => {
                self.record_attempt(ctx, &identifier, source, kind.code(), now)
                    .await?;
                self.emit_sign_in_failed(ctx, Some(user.id), kind.sign_in_reason())
                    .await?;
                return Err(LoginError::new(kind, "").to_api_error_traced(ctx.request_id.as_str()));
            }
            Err(kind) => {
                self.fail(ctx, &account, &identifier, source, Some(user.id), kind, now)
                    .await?;
                return Err(LoginError::new(kind, "").to_api_error_traced(ctx.request_id.as_str()));
            }
        }

        self.throttle
            .record_success(&account, source, now)
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;

        let key = user_key(user.id)
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        let started = self
            .sessions
            .start(key, session_source, ClientInfo::default(), now)
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;
        let token = started.token.as_str().to_owned();
        let session_id = started.session.id.clone();
        let expires_at = started.session.absolute_expires;

        self.record_attempt(ctx, &identifier, source, RESULT_OK, now)
            .await?;
        self.emit(
            "identity_session_started",
            Some(user.id),
            serde_json::json!({
                "session": session_id,
                "user": user.id.to_string(),
                "method": "password",
                "expires_at": format_time(expires_at)?,
            }),
        )
        .await?;
        self.emit(
            "identity_sign_in_succeeded",
            Some(user.id),
            serde_json::json!({
                "user": user.id.to_string(),
                "method": "password",
                "session": session_id,
                "step_up": false,
            }),
        )
        .await?;

        Ok(Session {
            token,
            expires_at: format_time(expires_at)?,
            user: UserInfo {
                id: user.id,
                email: user.email.unwrap_or_default(),
                full_name: user.full_name,
            },
        })
    }

    /// Verifies the second factor when the user has one. `Ok(Ok(()))` when none is required, so
    /// the caller can tell "no factor" from "a factor failed".
    async fn check_second_factor(
        &self,
        ctx: &RequestCtx,
        user: &mut crate::model::UserRecord,
        body: &LoginBody,
    ) -> Result<Result<(), LoginErrorKind>, ApiError> {
        let Some(secret) = user.totp_secret.take() else {
            return Ok(Ok(()));
        };
        match (&body.totp, &body.recovery_code) {
            (Some(code), _) => {
                match self
                    .totp
                    .verify(&secret, code, self.clock.as_ref(), user.totp_last_step)
                {
                    Ok(step) => {
                        match self.users.record_totp_step(user.id, step).await {
                            Ok(()) => Ok(Ok(())),
                            // Lost a race with a parallel sign-in using the same code.
                            Err(error) if error.kind == LoginErrorKind::TotpReplayed => {
                                Ok(Err(LoginErrorKind::TotpReplayed))
                            }
                            Err(error) => Err(error.to_api_error_traced(ctx.request_id.as_str())),
                        }
                    }
                    Err(AuthError::TotpReplayed) => Ok(Err(LoginErrorKind::TotpReplayed)),
                    Err(_) => Ok(Err(LoginErrorKind::TotpInvalid)),
                }
            }
            (None, Some(code)) => match verify_recovery_code(code, &user.recovery_digests) {
                Some(index) => {
                    self.users
                        .consume_recovery_digest(user.id, index)
                        .await
                        .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
                    Ok(Ok(()))
                }
                None => Ok(Err(LoginErrorKind::RecoveryCodeInvalid)),
            },
            (None, None) => Ok(Err(LoginErrorKind::TotpRequired)),
        }
    }

    /// Records a failure, counts it against the throttle and audits the true reason.
    async fn fail(
        &self,
        ctx: &RequestCtx,
        account: &AccountKey,
        identifier: &str,
        source: &str,
        user: Option<RecordId>,
        kind: LoginErrorKind,
        now: i64,
    ) -> Result<(), ApiError> {
        self.record_attempt(ctx, identifier, source, kind.code(), now)
            .await?;
        self.throttle
            .record_failure(account, source, now)
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;
        self.emit_sign_in_failed(ctx, user, kind.sign_in_reason())
            .await
    }

    async fn emit_sign_in_failed(
        &self,
        _ctx: &RequestCtx,
        user: Option<RecordId>,
        reason: &'static str,
    ) -> Result<(), ApiError> {
        let mut data = serde_json::Map::new();
        if let Some(id) = user {
            data.insert("user".to_owned(), serde_json::Value::String(id.to_string()));
        }
        data.insert(
            "method".to_owned(),
            serde_json::Value::String("password".to_owned()),
        );
        data.insert(
            "reason".to_owned(),
            serde_json::Value::String(reason.to_owned()),
        );
        self.emit(
            "identity_sign_in_failed",
            user,
            serde_json::Value::Object(data),
        )
        .await
    }

    pub(crate) async fn emit(
        &self,
        topic: &'static str,
        subject: Option<RecordId>,
        data: serde_json::Value,
    ) -> Result<(), ApiError> {
        self.events
            .emit(IdentityEvent {
                topic,
                subject,
                data,
            })
            .await
            .map_err(|error| error.to_api_error())
    }

    async fn record_attempt(
        &self,
        ctx: &RequestCtx,
        account: &str,
        source: &str,
        result_code: &str,
        now: i64,
    ) -> Result<(), ApiError> {
        let at = OffsetDateTime::from_unix_timestamp(now).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        self.events
            .record_login_attempt(LoginAttempt {
                site: ctx.site.clone(),
                account: account.to_owned(),
                source_digest: self.salt.digest_source(source).to_string(),
                result_code: result_code.to_owned(),
                at,
            })
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))
    }

    async fn logout_inner(&self, ctx: &RequestCtx) -> Result<(), ApiError> {
        let credential = ctx
            .credential
            .as_ref()
            .ok_or_else(|| ApiError::from(Code::AuthRequired))?;
        let token = credential.token.as_str();
        let now = self.clock.now_utc().unix_timestamp();
        if let Ok(presented) = spot_auth::OpaqueToken::parse(token) {
            let digest = presented.digest();
            if let Some(session) = self
                .sessions
                .store()
                .find_by_digest(&digest)
                .await
                .map_err(|error| {
                    LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
                })?
            {
                let user_id = crate::authenticator::record_id_from_text(session.user.as_str());
                let session_id = session.id.clone();
                self.sessions.end(&session_id, now).await.map_err(|error| {
                    LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
                })?;
                self.authenticator.invalidate_token(token);
                if let Some(user_id) = user_id {
                    self.emit(
                        "identity_session_ended",
                        Some(user_id),
                        serde_json::json!({
                            "session": session_id,
                            "user": user_id.to_string(),
                            "reason": "sign_out",
                        }),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn session_inner(&self, ctx: &RequestCtx) -> Result<SessionInfo, ApiError> {
        let principal = self.authenticator.authenticate(ctx).await?;
        // Recorded security finding (the architecture owner, 2026-11-10): an API key never reads the session, whatever its expiry,
        // as the other session routes refuse it. A key has no session to describe, and one with no
        // `expires_at` has no honest value for the contract's time string. Fail closed.
        if principal.session_id.is_none() {
            return Err(ApiError::from(Code::PermissionDenied));
        }
        let user = self
            .users
            .find_by_id(principal.user_id)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?
            .ok_or_else(|| ApiError::from(Code::AuthSessionExpired))?;
        Ok(SessionInfo {
            expires_at: format_time(principal.expires_at)?,
            user: UserInfo {
                id: user.id,
                email: user.email.unwrap_or_default(),
                full_name: user.full_name,
            },
            site: ctx.site.to_string(),
        })
    }

    /// Changes the caller's password: verifies the current one, enforces the policy, stores the
    /// new hash and ends every *other* session.
    ///
    /// # Errors
    ///
    /// `auth.invalid_credentials` for a wrong current password; `auth.password_policy` with one
    /// field per violated rule.
    pub async fn change_password(
        &self,
        ctx: &RequestCtx,
        current: &str,
        new: &str,
    ) -> Result<(), ApiError> {
        let principal = self.authenticator.authenticate(ctx).await?;
        let user = self
            .users
            .find_by_id(principal.user_id)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?
            .ok_or_else(invalid_credentials)?;

        let current_password = Password::new(current);
        let matches = user
            .password_hash
            .as_ref()
            .is_some_and(|hash| self.hasher.verify(&current_password, hash));
        if !matches {
            return Err(LoginError::new(LoginErrorKind::CurrentPasswordWrong, "")
                .to_api_error_traced(ctx.request_id.as_str()));
        }

        let new_password = Password::new(new);
        let context = [user.user_name.as_str(), user.email.as_deref().unwrap_or("")];
        if let Err(violations) = self.password_policy.check(&new_password, &context) {
            return Err(LoginError::password_policy(&violations));
        }

        let hash = self.hasher.hash(&new_password);
        self.users
            .set_password_hash(user.id, hash)
            .await
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;

        let key = user_key(user.id)
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        let report = self
            .sessions
            .revoke_all_for_user(
                &key,
                principal.session_id.as_deref(),
                SessionEndReason::PasswordChanged,
            )
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;
        self.authenticator.invalidate_all();

        self.emit(
            "identity_password_changed",
            Some(user.id),
            serde_json::json!({
                "user": user.id.to_string(),
                "by": "user",
                "sessions_ended": report.ended,
            }),
        )
        .await
    }

    /// Lists the caller's own sessions, oldest first.
    ///
    /// # Errors
    ///
    /// The shared authentication failures.
    pub async fn list_sessions(&self, ctx: &RequestCtx) -> Result<Vec<SessionSummary>, ApiError> {
        let principal = self.authenticator.authenticate(ctx).await?;
        let now = self.clock.now_utc().unix_timestamp();
        let key = user_key(principal.user_id)
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        let listed = self.sessions.list(&key, now).await.map_err(|error| {
            LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
        })?;
        Ok(listed
            .into_iter()
            .map(|session| SessionSummary {
                current: principal.session_id.as_deref() == Some(session.record.id.as_str()),
                id: session.record.id,
                live: session.live,
                created: session.record.created,
                last_seen: session.record.last_seen,
                expires_at: session.record.absolute_expires,
                source: session.source.to_string(),
                device: session.record.client.device,
            })
            .collect())
    }

    /// Ends one of the caller's own sessions.
    ///
    /// # Errors
    ///
    /// `auth.session_not_found` when no such session of the caller's exists — the same answer for
    /// another user's session, which is never revealed.
    pub async fn end_session(&self, ctx: &RequestCtx, session_id: &str) -> Result<(), ApiError> {
        let principal = self.authenticator.authenticate(ctx).await?;
        let key = user_key(principal.user_id)
            .map_err(|error| error.to_api_error_traced(ctx.request_id.as_str()))?;
        let now = self.clock.now_utc().unix_timestamp();
        let sessions = self
            .sessions
            .store()
            .list_for_user(&key)
            .await
            .map_err(|error| {
                LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
            })?;
        let Some(target) = sessions
            .into_iter()
            .find(|session| session.id == session_id)
        else {
            return Err(LoginError::new(LoginErrorKind::SessionNotFound, "")
                .to_api_error_traced(ctx.request_id.as_str()));
        };
        self.sessions.end(&target.id, now).await.map_err(|error| {
            LoginError::from(error).to_api_error_traced(ctx.request_id.as_str())
        })?;
        self.authenticator.invalidate_all();
        self.emit(
            "identity_session_ended",
            Some(principal.user_id),
            serde_json::json!({
                "session": target.id,
                "user": principal.user_id.to_string(),
                "reason": "revoked",
            }),
        )
        .await
    }
}

impl AuthService for LoginService {
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        // The edge resolved this address already (the connection's, or the forwarded client's only
        // through a configured trusted proxy): no header is read here. With no address the source
        // is unknown, which the throttle buckets per account rather than per caller.
        let from = match ctx.peer {
            Some(peer) => LoginSource::peer(peer.to_string()),
            None => LoginSource::unknown(),
        };
        Box::pin(async move { self.login_from(ctx, body, &from).await })
    }

    fn logout<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move { self.logout_inner(ctx).await })
    }

    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async move { self.session_inner(ctx).await })
    }
}

/// Trims and lower-cases a sign-in name for case-insensitive lookup and throttling.
#[must_use]
pub fn normalise_identifier(identifier: &str) -> String {
    identifier.trim().to_lowercase()
}

/// Formats a unix-second instant as the contract's UTC RFC 3339 string.
///
/// # Errors
///
/// `http.internal` for an instant outside the representable range (only reachable from corrupt
/// stored data).
pub fn format_time(unix_seconds: i64) -> Result<String, ApiError> {
    let instant = OffsetDateTime::from_unix_timestamp(unix_seconds)
        .map_err(|_| ApiError::from(Code::HttpInternal))?;
    instant
        .format(&Rfc3339)
        .map_err(|_| ApiError::from(Code::HttpInternal))
}

impl LoginErrorKind {
    /// The audit reason a sign-in failure carries (`identity_sign_in_failed.reason`).
    #[must_use]
    pub fn sign_in_reason(self) -> &'static str {
        match self {
            LoginErrorKind::InvalidIdentifier => "unknown_user",
            LoginErrorKind::AccountDisabled
            | LoginErrorKind::AccountInvited
            | LoginErrorKind::AccountAnonymised => "disabled",
            LoginErrorKind::AccountLocked
            | LoginErrorKind::Throttled
            | LoginErrorKind::ThrottleBurst => "locked",
            LoginErrorKind::TotpRequired => "policy",
            LoginErrorKind::StoreFailed => "policy",
            _ => "bad_credentials",
        }
    }
}
