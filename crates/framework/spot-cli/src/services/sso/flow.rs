//! The sign-in operations of the SSO host: begin, callback, ACS, hand-over and SAML metadata.
//!
//! Each operation follows the same order, and a failure at any step is a refusal:
//!
//! 1. the attempt is admitted by the coordinator throttle (charged to the provider and the source,
//!    never to an unverified account claim);
//! 2. the provider is looked up for the caller's tenant only (an unknown, disabled or other-tenant
//!    provider is one answer);
//! 3. the tenant policy must allow single sign-on and trust that provider;
//! 4. the pending flow is taken **once** from the store (a store error, a missing entry, an expired
//!    one, another provider's: all `StateInvalid` or `Unavailable`, never a pass);
//! 5. `spot-sso` checks state, PKCE, signature, issuer, audience, nonce and clock;
//! 6. the identity is resolved for the provider's audience and handed to the identity port;
//! 7. the outcome is audited with ids and codes only.
//!
//! Nothing here logs or returns a token, a code, a state, a nonce, a verifier, an assertion or a
//! claim value.

use spot_core::Audience;
use spot_http::api::ports::sso::{
    AcsParams, Begin, CallbackParams, Handover, ProviderList, ProviderPublic, SignedIn, SsoFailure,
};
use spot_http::api::service::RequestCtx;
use spot_secrets::SecretString;
use spot_sso::saml::{BeginOptions, SamlPendingAuth};
use spot_sso::{
    CallbackNext, ClaimsMap, ClientAuth, CompleteError, Discovery, KeyCache, PendingAuth,
    ProviderConfig, ProviderId, Resolution, SignInMode, SignInPolicy, SsoIdentity, TokenResponse,
    UserOptions, codes, parse_jwks, resolve_for_audience,
};
use time::OffsetDateTime;

use super::ports::{IdentityFailure, SignInRequest, SsoAuditEvent};
use super::{Kind, SsoHost};

/// The most providers a sign-in page is offered.
const MAX_OFFERED: usize = 20;

/// The `SsoFailure` an `SsoError` code is answered with. The layer renders the registry's generic
/// text for it, so the response never says which check failed; the audit keeps the real code.
#[must_use]
pub fn failure_for(code: &str) -> SsoFailure {
    match code {
        codes::STATE_MISMATCH | codes::STATE_EXPIRED | codes::STATE_REPLAYED => {
            SsoFailure::StateInvalid
        }
        codes::MISSING_PARAM | codes::BAD_RETURN_TO => SsoFailure::RequestInvalid,
        codes::PENDING_STORE_UNAVAILABLE
        | codes::PROVIDER_STORE_UNAVAILABLE
        | codes::REPLAY_STORE_UNAVAILABLE => SsoFailure::Unavailable,
        codes::SSO_NOT_ALLOWED
        | codes::WRONG_AUDIENCE
        | codes::USER_DISABLED
        | codes::UNKNOWN_IDENTITY
        | codes::LINK_REQUIRES_PROOF
        | codes::CONSUMER_CANNOT_LINK_USER => SsoFailure::NotAllowed,
        codes::REQUIRED => SsoFailure::Required,
        _ => SsoFailure::SignInFailed,
    }
}

/// A configuration that carries only the id and the audience, for `resolve_for_audience` when the
/// provider is a SAML one (that function reads nothing else of it).
fn identity_shadow(id: &ProviderId, audience: Audience) -> ProviderConfig {
    ProviderConfig {
        id: id.clone(),
        issuer: String::new(),
        client_id: String::new(),
        auth: ClientAuth::ClientSecretRef {
            secret_ref: String::new(),
        },
        scopes: Vec::new(),
        redirect_uri: String::new(),
        discovery_url: String::new(),
        allowed_algs: Vec::new(),
        clock_skew_seconds: 0,
        max_logout_token_lifetime_seconds: 120,
        claims_map: ClaimsMap::default(),
        require_email_verified: false,
        acr_values: Vec::new(),
        audience,
        user_options: UserOptions::default(),
    }
}

impl SsoHost {
    /// The injected current time, in unix seconds.
    pub fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Admits one attempt through the coordinator throttle. A throttled attempt and an outage are
    /// both refusals.
    async fn admit(&self, ctx: &RequestCtx, account: &str) -> Result<(), SsoFailure> {
        let peer = ctx.peer.map(|p| p.to_string());
        match self
            .admission
            .admit(&self.site, account, peer.as_deref())
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if error.status().as_u16() == 429 => Err(SsoFailure::SignInFailed),
            Err(_) => Err(SsoFailure::Unavailable),
        }
    }

    /// Records one outcome. A failure to record a refusal changes nothing (the sign-in is already
    /// refused); a failure to record a success is returned.
    async fn audit(
        &self,
        ctx: &RequestCtx,
        provider: Option<&str>,
        operation: &'static str,
        code: &'static str,
        accepted: bool,
    ) -> Result<(), SsoFailure> {
        let event = SsoAuditEvent {
            provider: provider.map(str::to_owned),
            operation,
            code,
            accepted,
            correlation_id: ctx.request_id.as_str().to_owned(),
            sessions_ended: None,
        };
        let recorded = self.collab.audit.record(ctx, event).await;
        if accepted && recorded.is_err() {
            return Err(SsoFailure::Unavailable);
        }
        Ok(())
    }

    /// Audits a refusal and returns it.
    async fn refuse(
        &self,
        ctx: &RequestCtx,
        provider: Option<&str>,
        operation: &'static str,
        code: &'static str,
        failure: SsoFailure,
    ) -> SsoFailure {
        let _ = self.audit(ctx, provider, operation, code, false).await;
        failure
    }

    /// The tenant's sign-in policy. A missing identity port or an error is a refusal: a policy
    /// that cannot be read never means "local".
    async fn policy(&self, ctx: &RequestCtx) -> Result<SignInPolicy, SsoFailure> {
        let port = self
            .collab
            .identity
            .as_ref()
            .ok_or(SsoFailure::Unavailable)?;
        port.policy(ctx).await.map_err(|_| SsoFailure::Unavailable)
    }

    /// The provider `id` of this tenant, OIDC first, then SAML.
    pub async fn kind(&self, ctx: &RequestCtx, id: &str) -> Result<Kind, SsoFailure> {
        if id.is_empty() || id.len() > 64 {
            return Err(SsoFailure::ProviderNotFound);
        }
        let provider = ProviderId::new(id);
        match self.stores.providers.get(&provider).await {
            Ok(Some(config)) => return Ok(Kind::Oidc(Box::new(config))),
            Ok(None) => {}
            // A row that no longer validates is not served.
            Err(spot_sso::ProviderStoreError::Invalid(_)) => {
                return Err(SsoFailure::ProviderNotFound);
            }
            Err(spot_sso::ProviderStoreError::Unavailable) => return Err(SsoFailure::Unavailable),
        }
        match self.collab.saml.get(ctx, &provider).await? {
            Some(saml) => Ok(Kind::Saml(Box::new(saml))),
            None => Err(SsoFailure::ProviderNotFound),
        }
    }

    /// Whether the policy allows single sign-on and trusts the provider.
    fn allowed(policy: &SignInPolicy, id: &ProviderId) -> bool {
        policy.mode != SignInMode::Local && policy.providers.contains(id)
    }

    /// The provider's discovery document and keys: cached, or fetched through the egress policy.
    /// `force` skips the cache (the caller has already been allowed a refetch).
    pub async fn material(
        &self,
        config: &ProviderConfig,
        force: bool,
    ) -> Result<(Discovery, KeyCache), SsoFailure> {
        let now = self.now();
        if !force && let Some(hit) = self.cache.get(config.id.as_str(), now) {
            return Ok(hit);
        }
        let unavailable = |_| SsoFailure::ProviderUnavailable;
        let body = self
            .fetcher
            .get(&self.site, &config.discovery_url)
            .await
            .map_err(unavailable)?;
        let discovery =
            Discovery::parse(&body, config).map_err(|_| SsoFailure::ProviderUnavailable)?;
        let jwks_body = self
            .fetcher
            .get(&self.site, &discovery.jwks_uri)
            .await
            .map_err(unavailable)?;
        let jwks = parse_jwks(&jwks_body).map_err(|_| SsoFailure::ProviderUnavailable)?;
        let keys = KeyCache::new(jwks, 3600);
        if !self
            .cache
            .put(config.id.as_str(), discovery.clone(), keys.clone(), now)
        {
            return Err(SsoFailure::ProviderUnavailable);
        }
        Ok((discovery, keys))
    }

    /// `sso_provider_list`.
    pub(super) async fn do_providers(&self, ctx: &RequestCtx) -> Result<ProviderList, SsoFailure> {
        let policy = self.policy(ctx).await?;
        if policy.mode == SignInMode::Local {
            return Ok(ProviderList {
                providers: Vec::new(),
                password_login: true,
            });
        }
        let listing = self
            .stores
            .providers
            .list()
            .await
            .map_err(|_| SsoFailure::Unavailable)?;
        let mut providers: Vec<ProviderPublic> = listing
            .providers
            .iter()
            .filter(|p| policy.providers.contains(&p.id))
            .map(|p| ProviderPublic {
                id: p.id.as_str().to_owned(),
                name: p.id.as_str().to_owned(),
                kind: "oidc".to_owned(),
            })
            .collect();
        for (id, name) in self.collab.saml.list(ctx).await? {
            if policy.providers.contains(&id) {
                providers.push(ProviderPublic {
                    id: id.as_str().to_owned(),
                    name,
                    kind: "saml".to_owned(),
                });
            }
        }
        providers.truncate(MAX_OFFERED);
        Ok(ProviderList {
            providers,
            password_login: policy.mode != SignInMode::SsoRequired,
        })
    }

    /// `sso_begin`.
    pub(super) async fn do_begin(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        return_to: Option<&str>,
    ) -> Result<Begin, SsoFailure> {
        self.admit(ctx, provider).await?;
        let kind = self.kind(ctx, provider).await?;
        let id = kind.id();
        let policy = self.policy(ctx).await?;
        if !Self::allowed(&policy, &id) {
            return Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    "begin",
                    codes::SSO_NOT_ALLOWED,
                    SsoFailure::NotAllowed,
                )
                .await);
        }
        let now = self.now();
        let return_to = return_to.unwrap_or("/");
        let (url, pending) = match &kind {
            Kind::Oidc(config) => {
                let (discovery, _) = self.material(config, false).await?;
                let (request, pending) = spot_sso::begin(
                    config,
                    &discovery,
                    &super::stores::begin_entropy(),
                    now,
                    return_to,
                )
                .map_err(|e| failure_for(e.code()))?;
                (request.url, pending)
            }
            Kind::Saml(saml) => {
                let (request, pending) = spot_sso::saml::begin(
                    &saml.sp,
                    &saml.idp,
                    &super::stores::begin_entropy(),
                    now,
                    return_to,
                    BeginOptions::default(),
                )
                .map_err(|e| failure_for(e.code()))?;
                if request.signing.is_some() {
                    // Signing the redirect needs the SP key from the secrets service.
                    return Err(self
                        .refuse(
                            ctx,
                            Some(provider),
                            "begin",
                            codes::SAML_SIGNING_KEY_MISSING,
                            SsoFailure::ProviderUnavailable,
                        )
                        .await);
                }
                (request.url, saml_to_pending(&pending, &saml.sp.acs_url))
            }
        };
        let binding = super::stores::binding_value();
        self.stores
            .pending
            .put(&binding, &pending)
            .await
            .map_err(|_| SsoFailure::Unavailable)?;
        self.audit(ctx, Some(provider), "begin", "sso.begun", true)
            .await?;
        Ok(Begin {
            redirect_url: url,
            binding,
        })
    }

    /// Takes the pending flow of `binding` once, for `provider`.
    async fn take_pending(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        operation: &'static str,
        binding: &str,
    ) -> Result<PendingAuth, SsoFailure> {
        let now = self.now();
        let taken = match self.stores.pending.take(binding, now).await {
            Ok(taken) => taken,
            Err(error) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider),
                        operation,
                        error.code(),
                        SsoFailure::Unavailable,
                    )
                    .await);
            }
        };
        match taken {
            Some(pending) if pending.provider.as_str() == provider => Ok(pending),
            _ => Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    operation,
                    codes::STATE_MISMATCH,
                    SsoFailure::StateInvalid,
                )
                .await),
        }
    }

    /// The client secret, read by name at the moment of use.
    async fn client_secret(
        &self,
        ctx: &RequestCtx,
        config: &ProviderConfig,
    ) -> Result<SecretString, SsoFailure> {
        let ClientAuth::ClientSecretRef { secret_ref } = &config.auth else {
            // private_key_jwt needs the signing key: not hosted yet.
            return Err(SsoFailure::ProviderUnavailable);
        };
        let secrets = self
            .collab
            .secrets
            .as_ref()
            .ok_or(SsoFailure::ProviderUnavailable)?;
        secrets
            .client_secret(ctx, secret_ref)
            .await
            .ok_or(SsoFailure::ProviderUnavailable)
    }

    /// `sso_callback`.
    pub(super) async fn do_callback(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        params: &CallbackParams,
    ) -> Result<Handover, SsoFailure> {
        self.admit(ctx, provider).await?;
        let Kind::Oidc(config) = self.kind(ctx, provider).await? else {
            return Err(SsoFailure::ProviderNotFound);
        };
        let policy = self.policy(ctx).await?;
        if !Self::allowed(&policy, &config.id) {
            return Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    "callback",
                    codes::SSO_NOT_ALLOWED,
                    SsoFailure::NotAllowed,
                )
                .await);
        }
        let pending = self
            .take_pending(ctx, provider, "callback", &params.binding)
            .await?;
        if params.error.is_some() {
            return Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    "callback",
                    codes::IDP_ERROR,
                    SsoFailure::SignInFailed,
                )
                .await);
        }
        let now = self.now();
        let (discovery, keys) = self.material(&config, false).await?;
        let oidc_params = spot_sso::CallbackParams {
            state: params.state.clone(),
            code: params.code.clone(),
            error: params.error.clone(),
            error_description: None,
        };
        let next = match spot_sso::callback(&config, &discovery, &pending, &oidc_params, now) {
            Ok(next) => next,
            Err(error) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider),
                        "callback",
                        error.code(),
                        failure_for(error.code()),
                    )
                    .await);
            }
        };
        let CallbackNext::ExchangeCode { request } = next;
        let secret = self.client_secret(ctx, &config).await?;
        let mut form = request.form;
        form.push(("client_secret".to_owned(), secret.expose().to_owned()));
        let body = self
            .fetcher
            .post_form(&self.site, &request.endpoint, &form)
            .await;
        drop(form);
        let body = match body {
            Ok(body) => body,
            Err(_) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider),
                        "callback",
                        codes::IDP_ERROR,
                        SsoFailure::ProviderUnavailable,
                    )
                    .await);
            }
        };
        let tokens = match TokenResponse::from_json(&body) {
            Ok(tokens) => tokens,
            Err(error) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider),
                        "callback",
                        error.code(),
                        SsoFailure::SignInFailed,
                    )
                    .await);
            }
        };
        let identity = self
            .verify_oidc(ctx, &config, &discovery, keys, &pending, &tokens)
            .await?;
        self.finish(
            ctx,
            "callback",
            &config.id,
            config.audience,
            &identity,
            &pending.return_to,
            None,
        )
        .await
    }

    /// Verifies the ID token; on an unknown `kid` refetches the keys once, if the refetch limit
    /// allows it.
    async fn verify_oidc(
        &self,
        ctx: &RequestCtx,
        config: &ProviderConfig,
        discovery: &Discovery,
        keys: KeyCache,
        pending: &PendingAuth,
        tokens: &TokenResponse,
    ) -> Result<SsoIdentity, SsoFailure> {
        let provider = config.id.as_str();
        let now = self.now();
        let mut outcome = self
            .verifier
            .oidc(config, discovery, &keys, pending, tokens, now);
        if matches!(outcome, Err(CompleteError::NeedFreshJwks { .. }))
            && self.cache.allow_refetch(provider, now)
        {
            let (discovery, fresh) = self.material(config, true).await?;
            outcome = self
                .verifier
                .oidc(config, &discovery, &fresh, pending, tokens, now);
        }
        match outcome {
            Ok(identity) => Ok(identity),
            Err(error) => Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    "callback",
                    error.code(),
                    failure_for(error.code()),
                )
                .await),
        }
    }

    /// `sso_acs`.
    pub(super) async fn do_acs(
        &self,
        ctx: &RequestCtx,
        provider: &str,
        params: &AcsParams,
    ) -> Result<Handover, SsoFailure> {
        self.admit(ctx, provider).await?;
        let Kind::Saml(saml) = self.kind(ctx, provider).await? else {
            return Err(SsoFailure::ProviderNotFound);
        };
        let id = ProviderId::new(provider);
        let policy = self.policy(ctx).await?;
        if !Self::allowed(&policy, &id) {
            return Err(self
                .refuse(
                    ctx,
                    Some(provider),
                    "acs",
                    codes::SSO_NOT_ALLOWED,
                    SsoFailure::NotAllowed,
                )
                .await);
        }
        let pending = self
            .take_pending(ctx, provider, "acs", &params.binding)
            .await?;
        let saml_pending = pending_to_saml(&pending);
        let now = self.now();
        let signed_in = self
            .verifier
            .saml(
                &saml.sp,
                &saml.idp,
                &saml_pending,
                &params.saml_response,
                params.relay_state.as_deref(),
                now,
                &self.stores.replay,
            )
            .await;
        let signed_in = match signed_in {
            Ok(signed_in) => signed_in,
            Err(error) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider),
                        "acs",
                        error.code(),
                        failure_for(error.code()),
                    )
                    .await);
            }
        };
        self.finish(
            ctx,
            "acs",
            &id,
            saml.audience,
            &signed_in.identity,
            &pending.return_to,
            signed_in.session_not_on_or_after,
        )
        .await
    }

    /// Resolves a verified identity for the provider's audience and hands it to the identity port.
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        ctx: &RequestCtx,
        operation: &'static str,
        provider: &ProviderId,
        audience: Audience,
        identity: &SsoIdentity,
        return_to: &str,
        session_not_on_or_after: Option<i64>,
    ) -> Result<Handover, SsoFailure> {
        let port = self
            .collab
            .identity
            .as_ref()
            .ok_or(SsoFailure::Unavailable)?;
        let snapshot = match port.snapshot(ctx, identity).await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider.as_str()),
                        operation,
                        codes::PROVIDER_STORE_UNAVAILABLE,
                        SsoFailure::Unavailable,
                    )
                    .await);
            }
        };
        let Some(policy) = snapshot.policy.clone() else {
            return Err(self
                .refuse(
                    ctx,
                    Some(provider.as_str()),
                    operation,
                    codes::PROVIDER_STORE_UNAVAILABLE,
                    SsoFailure::Unavailable,
                )
                .await);
        };
        let shadow = identity_shadow(provider, audience);
        let now = self.now();
        let at = OffsetDateTime::from_unix_timestamp(now).map_err(|_| SsoFailure::Unavailable)?;
        // The listener is the provider's audience (the request carries none): the check that
        // remains is the user type against that audience, inside `resolve_for_audience`.
        let resolution = resolve_for_audience(&shadow, audience, identity, &snapshot, &policy, at);
        match &resolution {
            Resolution::Denied(error) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider.as_str()),
                        operation,
                        error.code(),
                        failure_for(error.code()),
                    )
                    .await);
            }
            Resolution::RequireLocalConfirmation { .. } => {
                // An e-mail match alone never links.
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider.as_str()),
                        operation,
                        codes::LINK_REQUIRES_PROOF,
                        SsoFailure::NotAllowed,
                    )
                    .await);
            }
            _ => {}
        }
        let request = SignInRequest {
            ctx,
            provider,
            audience,
            identity,
            resolution: &resolution,
            return_to,
            session_not_on_or_after,
            now,
        };
        let handover = match port.sign_in(request).await {
            Ok(handover) => handover,
            Err(IdentityFailure::Refused(code)) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider.as_str()),
                        operation,
                        code,
                        failure_for(code),
                    )
                    .await);
            }
            Err(IdentityFailure::Unavailable) => {
                return Err(self
                    .refuse(
                        ctx,
                        Some(provider.as_str()),
                        operation,
                        codes::PROVIDER_STORE_UNAVAILABLE,
                        SsoFailure::Unavailable,
                    )
                    .await);
            }
        };
        self.audit(
            ctx,
            Some(provider.as_str()),
            operation,
            "sso.signed_in",
            true,
        )
        .await?;
        Ok(handover)
    }

    /// `sso_complete`.
    pub(super) async fn do_complete(
        &self,
        ctx: &RequestCtx,
        handover: &str,
    ) -> Result<SignedIn, SsoFailure> {
        self.admit(ctx, "handover").await?;
        let port = self
            .collab
            .identity
            .as_ref()
            .ok_or(SsoFailure::Unavailable)?;
        match port.redeem(ctx, handover).await {
            Ok(signed_in) => {
                self.audit(ctx, None, "complete", "sso.signed_in", true)
                    .await?;
                Ok(signed_in)
            }
            Err(IdentityFailure::Refused(code)) => Err(self
                .refuse(ctx, None, "complete", code, SsoFailure::SignInFailed)
                .await),
            Err(IdentityFailure::Unavailable) => Err(self
                .refuse(
                    ctx,
                    None,
                    "complete",
                    codes::PROVIDER_STORE_UNAVAILABLE,
                    SsoFailure::Unavailable,
                )
                .await),
        }
    }

    /// `sso_saml_metadata`.
    pub(super) async fn do_metadata(
        &self,
        ctx: &RequestCtx,
        provider: &str,
    ) -> Result<String, SsoFailure> {
        match self.kind(ctx, provider).await? {
            Kind::Saml(saml) => Ok(spot_sso::saml::sp_metadata(&saml.sp)),
            Kind::Oidc(_) => Err(SsoFailure::ProviderNotFound),
        }
    }

    /// The start-up check of each configured OIDC provider: its issuer and URLs validate.
    /// Returns the ids that do not, so one bad provider degrades the service without stopping
    /// the others.
    pub(super) async fn bad_providers(&self) -> Result<Vec<String>, SsoFailure> {
        let listing = self
            .stores
            .providers
            .list()
            .await
            .map_err(|_| SsoFailure::Unavailable)?;
        let mut bad: Vec<String> = listing
            .skipped
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        for config in &listing.providers {
            if url::Url::parse(&config.discovery_url).map(|u| u.scheme() == "https") != Ok(true)
                || url::Url::parse(&config.issuer).map(|u| u.scheme() == "https") != Ok(true)
            {
                bad.push(config.id.as_str().to_owned());
            }
        }
        bad.sort();
        Ok(bad)
    }
}

/// A SAML pending flow stored in the PendingAuth record: the request id as `state`, the relay
/// state as `nonce`, and the ACS URL as the redirect URI. The PKCE fields are unused markers.
fn saml_to_pending(saml: &SamlPendingAuth, acs_url: &str) -> PendingAuth {
    PendingAuth {
        provider: saml.provider.clone(),
        state: saml.request_id.clone(),
        nonce: saml.relay_state.clone(),
        code_verifier: "saml".to_owned(),
        code_challenge: "saml".to_owned(),
        redirect_uri: acs_url.to_owned(),
        return_to: saml.return_to.clone(),
        created_at: saml.created_at,
        expires_at: saml.expires_at,
        used: false,
    }
}

/// The inverse of [`saml_to_pending`].
fn pending_to_saml(pending: &PendingAuth) -> SamlPendingAuth {
    SamlPendingAuth {
        provider: pending.provider.clone(),
        request_id: pending.state.clone(),
        relay_state: pending.nonce.clone(),
        return_to: pending.return_to.clone(),
        created_at: pending.created_at,
        expires_at: pending.expires_at,
        used: false,
    }
}
