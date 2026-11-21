//! Single logout receivers: OIDC back-channel logout and SAML single logout (WP-IAM-05.T13).
//!
//! A verified logout token or request ends the sessions of that identity (or that `sid`), once.
//! `spot-sso` verifies and names a target; this module hosts the receivers behind `SsoPort`
//! (installed into the [`SsoHost`] of `services/sso.rs`):
//!
//! 1. the input is bounded and cheap to refuse (nothing is fetched for an oversized body);
//! 2. the provider is looked up for the site's tenant only; an unknown provider, another tenant's
//!    provider, a bad signature, a replayed `jti`, a wrong audience, an expired token: **one answer**
//!    (`LogoutRejected`, 400, no detail), with the real code only in the audit;
//! 3. the token or request is verified by the library (`jti` and request ids claimed once in the
//!    replay store, which is PostgreSQL in production);
//! 4. the subject resolves to the linked identities of that provider; an identity the tenant does
//!    not know answers like a known one (200, nothing ended, nothing created);
//! 5. one call ends the sessions ([`SessionEnder`]): by `sid` when the message names one, otherwise
//!    every session of the identity, with end reason `sso_logout`, bounded by a cap, in one
//!    transaction, and the realtime bridge closes open sockets. API keys are never touched;
//! 6. one audit event per accepted logout (provider, sessions ended) and one per refusal (code only).
//!
//! Level: **optional**. Without a provider of [`SessionEnder`] and [`LogoutDirectory`] the service is
//! degraded and a logout is answered `503` (the provider retries); nothing is ended on an error.
//! `[services.sso_logout]`: `enabled` (default `true` when `[services.sso]` is on),
//! `max_sessions_per_user` (default 50, 1 to 1000).

use std::sync::Arc;

use axum::http::Method;
use serde::Deserialize;
use spot_http::api::ports::sso::{SloBinding, SloOutcome, SloParams, SsoFailure};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_iam_directory::LinkedIdentity;
use spot_sso::{
    Discovery, IdpConfig, KeyCache, LogoutTarget, ProviderConfig, ProviderId, ReplayStore,
    SpConfig, SsoError, codes, resolve_target,
};
use spot_storage_contract::RecordId;

use crate::services::sso::{Kind, SsoAuditEvent, SsoHost, SsoLogoutHandler};
use crate::services::{CheckFuture, Idle, Level, OpenPath, RateLimit, SiteCtx};
use crate::{Error, Result};

/// Raised in this module's messages as `sso_logout_host.settings_invalid: ...`.
#[allow(dead_code)]
const CODE_SSO_LOGOUT_HOST_SETTINGS_INVALID: &str = "sso_logout_host.settings_invalid";

/// After `sso` (1010).
pub const ORDER: u32 = 1020;

/// The end reason recorded on a session ended by a logout.
pub const END_REASON: &str = "sso_logout";

/// The longest logout token or SAML message read, in bytes.
const MAX_MESSAGE: usize = 64 * 1024;

/// `[services.sso_logout]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the receivers are served.
    pub enabled: bool,
    /// The most sessions one logout ends per user (the per-user session cap).
    pub max_sessions_per_user: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_sessions_per_user: 50,
        }
    }
}

impl Settings {
    /// Checks the range.
    ///
    /// # Errors
    ///
    /// `sso_logout_host.settings_invalid`.
    pub fn validate(&self) -> Result<()> {
        if (1..=1000).contains(&self.max_sessions_per_user) {
            Ok(())
        } else {
            Err(Error(
                "sso_logout_host.settings_invalid: services.sso_logout.max_sessions_per_user is out of range"
                    .into(),
            ))
        }
    }
}

/// The directory or the session store could not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogoutUnavailable;

/// The identities linked to a provider subject, in the site's directory.
pub trait LogoutDirectory: Send + Sync + 'static {
    /// Every linked identity of `(issuer, subject)` for `ctx.site` (none for an unknown one).
    fn linked<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        issuer: &'a str,
        subject: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<LinkedIdentity>, LogoutUnavailable>>;
}

/// What to end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndPlan {
    /// The users whose sessions end (empty when the message names only a `sid`).
    pub users: Vec<RecordId>,
    /// The provider that said so; only sessions that began through it end.
    pub provider: ProviderId,
    /// The provider's session id, when the message names one session.
    pub session_id: Option<String>,
    /// The SAML `SessionIndex` values, when the request names particular sessions.
    pub session_indexes: Vec<String>,
    /// The end reason to record (always [`END_REASON`]).
    pub reason: &'static str,
    /// The most sessions to end per user.
    pub limit: usize,
}

/// Ends sessions. The implementation does it in one transaction, with end reason
/// [`END_REASON`], tells the realtime bridge so open sockets close at once, and leaves API keys
/// alone. When the plan has no user and a `sid`, it finds the session through its own `sid` index.
pub trait SessionEnder: Send + Sync + 'static {
    /// Ends the sessions of `plan` for `ctx.site`; the number of sessions ended.
    fn end<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        plan: &'a EndPlan,
    ) -> BoxFuture<'a, std::result::Result<u32, LogoutUnavailable>>;
}

/// The library's verification, as a seam so a unit test needs no signing key.
pub trait LogoutVerifier: Send + Sync + 'static {
    /// `spot_sso::validate_logout_token`.
    fn token<'a>(
        &'a self,
        config: &'a ProviderConfig,
        discovery: &'a Discovery,
        keys: &'a KeyCache,
        token: &'a str,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>>;

    /// `spot_sso::saml::validate_logout_request`.
    fn request<'a>(
        &'a self,
        sp: &'a SpConfig,
        idp: &'a IdpConfig,
        xml: &'a str,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>>;
}

/// The library's own checks.
#[derive(Clone, Copy, Debug, Default)]
pub struct LibraryLogoutVerifier;

impl LogoutVerifier for LibraryLogoutVerifier {
    fn token<'a>(
        &'a self,
        config: &'a ProviderConfig,
        discovery: &'a Discovery,
        keys: &'a KeyCache,
        token: &'a str,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>> {
        Box::pin(spot_sso::validate_logout_token(
            config, discovery, keys, token, now, replay,
        ))
    }

    fn request<'a>(
        &'a self,
        sp: &'a SpConfig,
        idp: &'a IdpConfig,
        xml: &'a str,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>> {
        Box::pin(spot_sso::saml::validate_logout_request(
            sp, idp, xml, now, replay,
        ))
    }
}

/// The receivers of one site.
pub struct SsoLogout {
    host: Arc<SsoHost>,
    directory: Arc<dyn LogoutDirectory>,
    ender: Arc<dyn SessionEnder>,
    verifier: Arc<dyn LogoutVerifier>,
    limit: usize,
}

impl SsoLogout {
    /// Receivers over `host` and their ports.
    #[must_use]
    pub fn new(
        host: Arc<SsoHost>,
        directory: Arc<dyn LogoutDirectory>,
        ender: Arc<dyn SessionEnder>,
        verifier: Arc<dyn LogoutVerifier>,
        limit: usize,
    ) -> Self {
        Self {
            host,
            directory,
            ender,
            verifier,
            limit,
        }
    }

    async fn audit(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        operation: &'static str,
        code: &'static str,
        ended: Option<u32>,
    ) {
        let event = SsoAuditEvent {
            provider: Some(provider.to_owned()),
            operation,
            code,
            accepted: ended.is_some(),
            correlation_id: ctx.request_id.as_str().to_owned(),
            sessions_ended: ended,
        };
        // A refusal or an acknowledgement needs no audit success: nothing signs in.
        let _ = self.host.collaborators().audit.record(ctx, event).await;
    }

    async fn reject(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        operation: &'static str,
        code: &'static str,
    ) -> SsoFailure {
        self.audit(ctx, provider, operation, code, None).await;
        SsoFailure::LogoutRejected
    }

    /// Resolves the verified target and ends the sessions. Unknown identities answer like known.
    async fn apply(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        operation: &'static str,
        target: LogoutTarget,
    ) -> std::result::Result<(), SsoFailure> {
        let mut users = Vec::new();
        if let Some(subject) = target.subject.as_deref().filter(|s| !s.is_empty()) {
            let linked = match self.directory.linked(ctx, &target.issuer, subject).await {
                Ok(linked) => linked,
                Err(_) => {
                    self.audit(
                        ctx,
                        provider,
                        operation,
                        codes::PROVIDER_STORE_UNAVAILABLE,
                        None,
                    )
                    .await;
                    return Err(SsoFailure::Unavailable);
                }
            };
            users = resolve_target(&target, &linked)
                .into_iter()
                .map(|identity| identity.user)
                .collect();
        }
        if users.is_empty() && target.session_id.is_none() {
            // Someone we do not know: acknowledged, nothing ended, nothing created.
            self.audit(ctx, provider, operation, "sso.logout_accepted", Some(0))
                .await;
            return Ok(());
        }
        let plan = EndPlan {
            users,
            provider: target.provider.clone(),
            session_id: target.session_id.clone(),
            session_indexes: target.session_indexes.clone(),
            reason: END_REASON,
            limit: self.limit,
        };
        match self.ender.end(ctx, &plan).await {
            Ok(ended) => {
                self.audit(ctx, provider, operation, "sso.logout_accepted", Some(ended))
                    .await;
                Ok(())
            }
            Err(_) => {
                self.audit(
                    ctx,
                    provider,
                    operation,
                    codes::PROVIDER_STORE_UNAVAILABLE,
                    None,
                )
                .await;
                Err(SsoFailure::Unavailable)
            }
        }
    }

    async fn back_channel(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        token: &str,
    ) -> std::result::Result<(), SsoFailure> {
        if token.is_empty() || token.len() > MAX_MESSAGE {
            return Err(self
                .reject(ctx, provider, "backchannel", codes::LOGOUT_TOKEN_NO_TARGET)
                .await);
        }
        let config = match self.host.kind(ctx, provider).await {
            Ok(Kind::Oidc(config)) => config,
            // Unknown, another tenant's, a SAML provider, a store outage: one answer.
            _ => {
                return Err(self
                    .reject(ctx, provider, "backchannel", codes::PROVIDER_NOT_CONFIGURED)
                    .await);
            }
        };
        let now = self.host.now();
        let replay = &self.host.stores().replay;
        let (discovery, keys) = match self.host.material(&config, false).await {
            Ok(material) => material,
            Err(_) => {
                return Err(self
                    .reject(ctx, provider, "backchannel", codes::DISCOVERY_MISMATCH)
                    .await);
            }
        };
        let mut outcome = self
            .verifier
            .token(&config, &discovery, &keys, token, now, replay)
            .await;
        if matches!(&outcome, Err(e) if e.code() == codes::JWKS_STALE)
            && self.host.may_refetch(provider)
            && let Ok((discovery, keys)) = self.host.material(&config, true).await
        {
            outcome = self
                .verifier
                .token(&config, &discovery, &keys, token, now, replay)
                .await;
        }
        match outcome {
            Ok(target) => self.apply(ctx, provider, "backchannel", target).await,
            Err(error) => {
                let unavailable = error.code() == codes::REPLAY_STORE_UNAVAILABLE;
                let failure = self
                    .reject(ctx, provider, "backchannel", error.code())
                    .await;
                Err(if unavailable {
                    SsoFailure::Unavailable
                } else {
                    failure
                })
            }
        }
    }

    async fn single(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        params: &SloParams,
    ) -> std::result::Result<SloOutcome, SsoFailure> {
        if let Some(response) = &params.saml_response {
            // We never start a logout at the provider, so a response answers nothing: consumed.
            let _ = response;
            return Ok(SloOutcome::Done);
        }
        let Some(message) = params.saml_request.as_deref() else {
            return Err(self
                .reject(ctx, provider, "slo", codes::SAML_SLO_NOT_A_REQUEST)
                .await);
        };
        if message.is_empty() || message.len() > MAX_MESSAGE {
            return Err(self
                .reject(ctx, provider, "slo", codes::SAML_TOO_LARGE)
                .await);
        }
        let Ok(Kind::Saml(saml)) = self.host.kind(ctx, provider).await else {
            return Err(self
                .reject(ctx, provider, "slo", codes::PROVIDER_NOT_CONFIGURED)
                .await);
        };
        let xml = match params.binding {
            SloBinding::Redirect => spot_sso::saml::request::decode_redirect(message).ok(),
            SloBinding::Post => base64_decode(message).and_then(|b| String::from_utf8(b).ok()),
        };
        let Some(xml) = xml else {
            return Err(self
                .reject(ctx, provider, "slo", codes::SAML_XML_INVALID)
                .await);
        };
        let now = self.host.now();
        let replay = &self.host.stores().replay;
        match self
            .verifier
            .request(&saml.sp, &saml.idp, &xml, now, replay)
            .await
        {
            Ok(target) => {
                self.apply(ctx, provider, "slo", target).await?;
                Ok(SloOutcome::Done)
            }
            Err(error) => {
                let unavailable = error.code() == codes::REPLAY_STORE_UNAVAILABLE;
                let failure = self.reject(ctx, provider, "slo", error.code()).await;
                Err(if unavailable {
                    SsoFailure::Unavailable
                } else {
                    failure
                })
            }
        }
    }
}

impl SsoLogoutHandler for SsoLogout {
    fn backchannel_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        logout_token: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), SsoFailure>> {
        Box::pin(self.back_channel(ctx, provider, logout_token))
    }

    fn single_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a SloParams,
    ) -> BoxFuture<'a, std::result::Result<SloOutcome, SsoFailure>> {
        Box::pin(self.single(ctx, provider, params))
    }
}

/// Standard Base64 (padding optional), `None` for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut have) = (0_u32, 0_u32);
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push(((bits >> have) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Joins the site: installs the receivers into the SSO host and declares their open paths.
///
/// # Errors
///
/// A setting out of range.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sso_logout")?;
    settings.validate()?;
    let Some(host) = ctx.port::<SsoHost>() else {
        // Single sign-on is not enabled on this site: nothing to receive.
        return Ok(());
    };
    if !settings.enabled {
        return Ok(());
    }
    let directory = ctx.port::<dyn LogoutDirectory>();
    let ender = ctx.port::<dyn SessionEnder>();
    let ready = directory.is_some() && ender.is_some();
    if ctx.assembling() {
        ctx.add(Idle::new(
            "sso_logout",
            Level::Optional,
            move || -> CheckFuture<'static> {
                Box::pin(async move {
                    if ready {
                        Ok(())
                    } else {
                        Err("no module provides the logout directory and the session ender, so every single logout is refused with 503".to_owned())
                    }
                })
            },
        ));
    }
    if let (Some(directory), Some(ender)) = (directory, ender) {
        host.install_logout(Arc::new(SsoLogout::new(
            Arc::clone(&host),
            directory,
            ender,
            Arc::new(LibraryLogoutVerifier),
            settings.max_sessions_per_user,
        )));
    }
    let limit = RateLimit { per_minute: 600 };
    ctx.open_paths(&[
        OpenPath {
            method: Method::POST,
            pattern: "/api/v1/auth/sso/{provider}/backchannel-logout",
            limit,
        },
        OpenPath {
            method: Method::GET,
            pattern: "/api/v1/auth/sso/{provider}/slo",
            limit,
        },
        OpenPath {
            method: Method::POST,
            pattern: "/api/v1/auth/sso/{provider}/slo",
            limit,
        },
    ])?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use spot_core::{Audience, ManualClock, SiteId};
    use spot_http::api::service::ApiError;
    use spot_http::request::RequestId;
    use spot_sso::contract_stores::sample_provider;
    use spot_sso::{
        CompleteError, MemoryPendingStore, MemoryProviderStore, MemoryReplayStore, PendingAuth,
        ProviderStore, SamlSignIn, TokenResponse,
    };
    use time::OffsetDateTime;

    use super::*;
    use crate::services::sso::{
        CacheLimits, Collaborators, FetchError, Fetcher, NoSamlProviders, SsoAdmission, Stores,
        TracingAudit, Verifier,
    };

    /// A 2048-bit modulus in base64url (0xC1 bytes, last byte 1), built without a base64 crate.
    fn modulus() -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bytes = vec![0xC1_u8; 256];
        bytes[255] = 1;
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }

    struct Net;

    impl Fetcher for Net {
        fn get<'a>(
            &'a self,
            _t: &'a str,
            url: &'a str,
        ) -> BoxFuture<'a, std::result::Result<Vec<u8>, FetchError>> {
            let body = if url.ends_with("openid-configuration") {
                serde_json::json!({
                    "issuer": "https://idp.example.com",
                    "authorization_endpoint": "https://idp.example.com/auth",
                    "token_endpoint": "https://idp.example.com/token",
                    "jwks_uri": "https://idp.example.com/jwks",
                    "code_challenge_methods_supported": ["S256"],
                    "response_types_supported": ["code"]
                })
                .to_string()
                .into_bytes()
            } else {
                let key = modulus();
                serde_json::json!({"keys": [{"kty": "RSA", "kid": "k1", "n": key, "e": "AQAB"}]})
                    .to_string()
                    .into_bytes()
            };
            Box::pin(async move { Ok(body) })
        }
        fn post_form<'a>(
            &'a self,
            _t: &'a str,
            _u: &'a str,
            _f: &'a [(String, String)],
        ) -> BoxFuture<'a, std::result::Result<Vec<u8>, FetchError>> {
            Box::pin(async { Err(FetchError::Refused) })
        }
    }

    struct Open;

    impl SsoAdmission for Open {
        fn admit<'a>(
            &'a self,
            _t: &'a str,
            _a: &'a str,
            _p: Option<&'a str>,
        ) -> BoxFuture<'a, std::result::Result<(), ApiError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct NoVerify;

    impl Verifier for NoVerify {
        fn oidc(
            &self,
            _c: &ProviderConfig,
            _d: &Discovery,
            _k: &KeyCache,
            _p: &PendingAuth,
            _t: &TokenResponse,
            _n: i64,
        ) -> std::result::Result<spot_sso::SsoIdentity, CompleteError> {
            Err(CompleteError::Refused(SsoError::new(
                codes::TOKEN_MALFORMED,
                "x",
                "x",
            )))
        }
        fn saml<'a>(
            &'a self,
            _s: &'a SpConfig,
            _i: &'a IdpConfig,
            _p: &'a spot_sso::saml::SamlPendingAuth,
            _r: &'a str,
            _rs: Option<&'a str>,
            _n: i64,
            _replay: &'a Arc<dyn ReplayStore>,
        ) -> BoxFuture<'a, std::result::Result<SamlSignIn, SsoError>> {
            Box::pin(async { Err(SsoError::new(codes::SAML_REPLAY, "x", "x")) })
        }
    }

    struct Verdict(Mutex<std::result::Result<LogoutTarget, &'static str>>);

    impl LogoutVerifier for Verdict {
        fn token<'a>(
            &'a self,
            _c: &'a ProviderConfig,
            _d: &'a Discovery,
            _k: &'a KeyCache,
            _t: &'a str,
            _n: i64,
            _r: &'a Arc<dyn ReplayStore>,
        ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>> {
            let v = self.0.lock().unwrap().clone();
            Box::pin(async move { v.map_err(|c| SsoError::new(c, "x", "x")) })
        }
        fn request<'a>(
            &'a self,
            _s: &'a SpConfig,
            _i: &'a IdpConfig,
            _x: &'a str,
            _n: i64,
            _r: &'a Arc<dyn ReplayStore>,
        ) -> BoxFuture<'a, std::result::Result<LogoutTarget, SsoError>> {
            let v = self.0.lock().unwrap().clone();
            Box::pin(async move { v.map_err(|c| SsoError::new(c, "x", "x")) })
        }
    }

    struct Dir(Vec<LinkedIdentity>);

    impl LogoutDirectory for Dir {
        fn linked<'a>(
            &'a self,
            _c: &'a RequestCtx,
            _i: &'a str,
            _s: &'a str,
        ) -> BoxFuture<'a, std::result::Result<Vec<LinkedIdentity>, LogoutUnavailable>> {
            let v = self.0.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    #[derive(Default)]
    struct Ender(Mutex<Vec<EndPlan>>);

    impl SessionEnder for Ender {
        fn end<'a>(
            &'a self,
            _c: &'a RequestCtx,
            plan: &'a EndPlan,
        ) -> BoxFuture<'a, std::result::Result<u32, LogoutUnavailable>> {
            self.0.lock().unwrap().push(plan.clone());
            let n = u32::try_from(plan.users.len()).unwrap_or(0) * 2;
            Box::pin(async move { Ok(n) })
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: SiteId::new("acme").unwrap(),
            credential: None,
            idempotency_key: None,
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    fn target(subject: Option<&str>, sid: Option<&str>) -> LogoutTarget {
        LogoutTarget::for_oidc(
            ProviderId::new("acme-idp"),
            "https://idp.example.com",
            subject.map(str::to_owned),
            sid.map(str::to_owned),
        )
    }

    struct Rig {
        logout: SsoLogout,
        host: Arc<SsoHost>,
        ender: Arc<Ender>,
        verdict: Arc<Verdict>,
        user: RecordId,
    }

    async fn rig() -> Rig {
        let providers = MemoryProviderStore::new().for_tenant("acme");
        providers
            .put(&sample_provider("acme-idp", Audience::Staff))
            .await
            .unwrap();
        let stores = Stores {
            providers: Arc::new(providers),
            pending: Arc::new(MemoryPendingStore::new().for_tenant("acme")),
            replay: Arc::new(MemoryReplayStore::new()),
            durable: false,
        };
        let collab = Collaborators {
            identity: None,
            secrets: None,
            audit: Arc::new(TracingAudit),
            saml: Arc::new(NoSamlProviders),
        };
        let host = Arc::new(SsoHost::new(
            "acme",
            Arc::new(ManualClock::new(
                OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
            )),
            stores,
            Arc::new(Net),
            CacheLimits::default(),
            collab,
            Arc::new(Open),
            Arc::new(NoVerify),
        ));
        let user = RecordId::new_v7();
        let ender = Arc::new(Ender::default());
        let verdict = Arc::new(Verdict(Mutex::new(Ok(target(Some("sub-1"), None)))));
        let linked = vec![LinkedIdentity {
            user,
            issuer: "https://idp.example.com".into(),
            subject: "sub-1".into(),
        }];
        let logout = SsoLogout::new(
            host.clone(),
            Arc::new(Dir(linked)),
            ender.clone(),
            verdict.clone(),
            50,
        );
        Rig {
            logout,
            host,
            ender,
            verdict,
            user,
        }
    }

    #[tokio::test]
    async fn a_verified_token_ends_the_identitys_sessions_once_with_the_cap() {
        let rig = rig().await;
        rig.logout
            .back_channel(&ctx(), "acme-idp", "token")
            .await
            .unwrap();
        let plans = rig.ender.0.lock().unwrap().clone();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].users, vec![rig.user]);
        assert_eq!((plans[0].reason, plans[0].limit), (END_REASON, 50));
        assert!(plans[0].session_id.is_none());
    }

    #[tokio::test]
    async fn a_sid_alone_is_handed_to_the_ender() {
        let rig = rig().await;
        *rig.verdict.0.lock().unwrap() = Ok(target(None, Some("sid-9")));
        rig.logout
            .back_channel(&ctx(), "acme-idp", "token")
            .await
            .unwrap();
        let plans = rig.ender.0.lock().unwrap().clone();
        assert!(plans[0].users.is_empty());
        assert_eq!(plans[0].session_id.as_deref(), Some("sid-9"));
    }

    #[tokio::test]
    async fn an_unknown_identity_is_acknowledged_and_nothing_ends() {
        let rig = rig().await;
        *rig.verdict.0.lock().unwrap() = Ok(target(Some("someone-else"), None));
        assert!(
            rig.logout
                .back_channel(&ctx(), "acme-idp", "token")
                .await
                .is_ok()
        );
        assert!(rig.ender.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_verification_failure_is_one_answer_and_ends_nothing() {
        for code in [
            codes::LOGOUT_TOKEN_REPLAYED,
            codes::LOGOUT_TOKEN_EXPIRED,
            codes::LOGOUT_TOKEN_NO_EXP,
            codes::LOGOUT_TOKEN_NONCE_PRESENT,
            codes::ALG_UNSUPPORTED,
            codes::SAML_SLO_EXPIRED,
        ] {
            let rig = rig().await;
            *rig.verdict.0.lock().unwrap() = Err(code);
            let refused = rig.logout.back_channel(&ctx(), "acme-idp", "token").await;
            assert_eq!(refused.unwrap_err(), SsoFailure::LogoutRejected, "{code}");
            assert!(rig.ender.0.lock().unwrap().is_empty());
        }
        // An unknown provider answers the same as a bad token.
        let rig = rig().await;
        let unknown = rig.logout.back_channel(&ctx(), "no-such", "token").await;
        assert_eq!(unknown.unwrap_err(), SsoFailure::LogoutRejected);
        // An oversized or empty token is refused before anything is fetched.
        let big = "x".repeat(MAX_MESSAGE + 1);
        assert_eq!(
            rig.logout
                .back_channel(&ctx(), "acme-idp", &big)
                .await
                .unwrap_err(),
            SsoFailure::LogoutRejected
        );
        assert_eq!(
            rig.logout
                .back_channel(&ctx(), "acme-idp", "")
                .await
                .unwrap_err(),
            SsoFailure::LogoutRejected
        );
        // A replay store outage is a retryable refusal, not a pass.
        *rig.verdict.0.lock().unwrap() = Err(codes::REPLAY_STORE_UNAVAILABLE);
        assert_eq!(
            rig.logout
                .back_channel(&ctx(), "acme-idp", "t")
                .await
                .unwrap_err(),
            SsoFailure::Unavailable
        );
    }

    #[tokio::test]
    async fn saml_single_logout_without_a_provider_or_request_is_rejected() {
        let rig = rig().await;
        let params = SloParams {
            binding: SloBinding::Post,
            saml_request: Some("AAAA".into()),
            saml_response: None,
            relay_state: None,
        };
        assert_eq!(
            rig.logout
                .single(&ctx(), "saml-idp", &params)
                .await
                .unwrap_err(),
            SsoFailure::LogoutRejected
        );
        let none = SloParams {
            binding: SloBinding::Post,
            saml_request: None,
            saml_response: None,
            relay_state: None,
        };
        assert_eq!(
            rig.logout
                .single(&ctx(), "saml-idp", &none)
                .await
                .unwrap_err(),
            SsoFailure::LogoutRejected
        );
        let response = SloParams {
            binding: SloBinding::Post,
            saml_request: None,
            saml_response: Some("x".into()),
            relay_state: None,
        };
        assert_eq!(
            rig.logout
                .single(&ctx(), "saml-idp", &response)
                .await
                .unwrap(),
            SloOutcome::Done
        );
        let _ = &rig.host;
    }

    #[test]
    fn base64_decodes_standard_text_and_refuses_the_rest() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert!(base64_decode("a b").is_none());
        assert!(base64_decode("a-b_").is_none());
    }

    #[test]
    fn the_cap_setting_is_bounded() {
        assert!(Settings::default().validate().is_ok());
        let zero = Settings {
            max_sessions_per_user: 0,
            ..Settings::default()
        };
        assert!(
            zero.validate()
                .unwrap_err()
                .to_string()
                .contains("sso_logout_host.settings_invalid")
        );
        assert!(toml::from_str::<Settings>("cap = 1").is_err());
    }
}
