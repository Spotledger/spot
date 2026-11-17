//! The seams of the SSO host: what it needs from the rest of the site, as small object-safe ports.
//!
//! `services/sso.rs` decides nothing about identity itself (`spot-sso` does); it moves bytes
//! through the egress policy, stores the pending flow, and hands the verified identity to these
//! ports. Each has a stub for tests and a production provider registered by another module through
//! the typed ports map (`ctx.provide::<dyn ...>`):
//!
//! - [`SsoIdentityPort`]: the tenant directory and the session. Reads a [`DirectorySnapshot`] for
//!   an identity, applies a resolution (link by invitation, just-in-time creation, a consumer
//!   principal) and issues the one-time hand-over, then redeems it for the session.
//! - [`SsoSecrets`]: the client secret, read by name at the moment of use.
//! - [`SsoAudit`]: one identity audit event per outcome, never a token, assertion or claim value.
//! - [`SamlProviders`]: the tenant's SAML providers (the provider store holds OIDC ones only).
//! - [`SsoLogoutHandler`]: the logout operations of `SsoPort`, installed by `sso_logout`.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_core::Audience;
use spot_http::api::ports::sso::{Handover, SignedIn, SloOutcome, SloParams, SsoFailure};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_secrets::SecretString;
use spot_sso::{
    DirectoryUser, DirectoryView, IdpConfig, ProviderId, Resolution, SignInPolicy, SpConfig,
    SsoIdentity,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

/// Why the identity port could not finish a sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityFailure {
    /// The policy or the directory refuses this person (the stable `sso.*` code, for the audit).
    Refused(&'static str),
    /// The directory or the session store could not answer: the sign-in is refused.
    Unavailable,
}

/// What the resolution reads about one identity, loaded once per sign-in for one tenant.
///
/// It implements [`DirectoryView`] over data the identity port prepared, so `spot_sso::resolve`
/// stays synchronous and sans-I/O. A cross-tenant row cannot be in it: the port loads only the
/// caller's tenant.
#[derive(Clone, Debug, Default)]
pub struct DirectorySnapshot {
    /// The tenant's sign-in policy.
    pub policy: Option<SignInPolicy>,
    /// The users the identity could resolve to.
    pub users: Vec<DirectoryUser>,
    /// Linked identities: `(issuer, subject)` to the user.
    pub linked: Vec<((String, String), RecordId)>,
    /// Open invitations: lower-case e-mail to the invitation.
    pub invitations: Vec<(String, RecordId)>,
    /// Users by lower-case sign-in e-mail.
    pub emails: Vec<(String, RecordId)>,
    /// How many enabled tenant administrators exist.
    pub tenant_admins: usize,
    /// How many enabled break-glass administrators exist.
    pub break_glass_admins: usize,
    /// The configured providers: id, healthy, audience.
    pub providers: BTreeMap<String, (bool, Audience)>,
}

impl DirectoryView for DirectorySnapshot {
    fn user(&self, id: RecordId) -> Option<DirectoryUser> {
        self.users.iter().find(|u| u.id == id).cloned()
    }
    fn enabled_tenant_admins(&self) -> usize {
        self.tenant_admins
    }
    fn enabled_break_glass_admins(&self) -> usize {
        self.break_glass_admins
    }
    fn provider_healthy(&self, provider: &ProviderId) -> bool {
        self.providers
            .get(provider.as_str())
            .is_some_and(|(healthy, _)| *healthy)
    }
    fn provider_audience(&self, provider: &ProviderId) -> Audience {
        self.providers
            .get(provider.as_str())
            .map_or(Audience::Staff, |(_, audience)| *audience)
    }
    fn user_for_email(&self, email: &str) -> Option<RecordId> {
        let email = email.to_ascii_lowercase();
        self.emails
            .iter()
            .find(|(e, _)| *e == email)
            .map(|(_, id)| *id)
    }
    fn linked_user(&self, issuer: &str, subject: &str) -> Option<RecordId> {
        self.linked
            .iter()
            .find(|((i, s), _)| i == issuer && s == subject)
            .map(|(_, id)| *id)
    }
    fn open_invitation(&self, email: &str, _now: OffsetDateTime) -> Option<RecordId> {
        let email = email.to_ascii_lowercase();
        self.invitations
            .iter()
            .find(|(e, _)| *e == email)
            .map(|(_, id)| *id)
    }
}

/// Everything the identity port needs to finish one sign-in.
pub struct SignInRequest<'a> {
    /// The request (site, correlation id, source).
    pub ctx: &'a RequestCtx,
    /// The provider the identity came from.
    pub provider: &'a ProviderId,
    /// The provider's audience: the session records it, and with `method = sso`.
    pub audience: Audience,
    /// The identity `spot-sso` verified.
    pub identity: &'a SsoIdentity,
    /// What to do with it; never [`Resolution::Denied`] (the host refuses those itself).
    pub resolution: &'a Resolution,
    /// The same-site path to send the browser to.
    pub return_to: &'a str,
    /// The SAML assertion's cap on the session, in unix seconds, when it set one.
    pub session_not_on_or_after: Option<i64>,
    /// The injected current time, in unix seconds.
    pub now: i64,
}

/// The tenant directory and the session, for the SSO host.
pub trait SsoIdentityPort: Send + Sync + 'static {
    /// The tenant's sign-in policy for `ctx.site`. An error is a refusal: a policy that cannot
    /// be read never means `local`.
    fn policy<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<SignInPolicy, IdentityFailure>>;

    /// The directory facts `resolve` needs about `identity`, for `ctx.site` only. An error is a
    /// refusal of the sign-in.
    fn snapshot<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        identity: &'a SsoIdentity,
    ) -> BoxFuture<'a, Result<DirectorySnapshot, IdentityFailure>>;

    /// Applies the resolution (links an invitation, creates the user with no roles, finds or
    /// creates the consumer principal) and issues the one-time hand-over for the session.
    fn sign_in<'a>(
        &'a self,
        request: SignInRequest<'a>,
    ) -> BoxFuture<'a, Result<Handover, IdentityFailure>>;

    /// Redeems the hand-over (once) for the native API session, recording `method = sso` and the
    /// audience on it.
    fn redeem<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, Result<SignedIn, IdentityFailure>>;
}

/// The secrets service, narrowed to reading one client secret by name.
pub trait SsoSecrets: Send + Sync + 'static {
    /// The value of the entry `name` for `ctx.site`, `None` when there is none or it cannot be
    /// read. Never logged by the host.
    fn client_secret<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Option<SecretString>>;
}

/// One identity audit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsoAuditEvent {
    /// The provider id, when the request named one that exists.
    pub provider: Option<String>,
    /// The operation (`begin`, `callback`, `acs`, `complete`, `metadata`).
    pub operation: &'static str,
    /// The stable code of the outcome (`sso.signed_in` on success).
    pub code: &'static str,
    /// Whether the attempt succeeded.
    pub accepted: bool,
    /// The request's correlation id.
    pub correlation_id: String,
    /// How many sessions the operation ended (a logout), when it ended any.
    pub sessions_ended: Option<u32>,
}

/// The audit trail could not take the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditUnavailable;

/// Where SSO outcomes are audited.
pub trait SsoAudit: Send + Sync + 'static {
    /// Records `event` for `ctx.site`. A sign-in whose success cannot be recorded is refused.
    fn record<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        event: SsoAuditEvent,
    ) -> BoxFuture<'a, Result<(), AuditUnavailable>>;
}

/// The default audit: a structured log line with ids and codes only.
#[derive(Clone, Copy, Debug, Default)]
pub struct TracingAudit;

impl SsoAudit for TracingAudit {
    fn record<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        event: SsoAuditEvent,
    ) -> BoxFuture<'a, Result<(), AuditUnavailable>> {
        Box::pin(async move {
            tracing::info!(
                site = ctx.site.as_str(),
                provider = event.provider.as_deref().unwrap_or(""),
                operation = event.operation,
                code = event.code,
                accepted = event.accepted,
                correlation_id = event.correlation_id.as_str(),
                "sso"
            );
            Ok(())
        })
    }
}

/// A SAML provider: what this tenant is to the IdP and what the IdP is.
#[derive(Clone, Debug)]
pub struct SamlProvider {
    /// The service provider configuration (entity id, ACS URL, signature policy).
    pub sp: SpConfig,
    /// The pinned identity provider configuration.
    pub idp: IdpConfig,
    /// The display name for the sign-in page.
    pub name: String,
    /// Who the provider signs in.
    pub audience: Audience,
}

/// The tenant's SAML providers. The provider store of `spot-sso-pg` holds OIDC providers only, so
/// this is its own port; until a module provides one, no SAML provider exists.
pub trait SamlProviders: Send + Sync + 'static {
    /// The SAML provider `id` of `ctx.site`, `None` for an unknown, disabled or other-tenant one.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a ProviderId,
    ) -> BoxFuture<'a, Result<Option<SamlProvider>, SsoFailure>>;

    /// The ids and display names to offer on the sign-in page.
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<Vec<(ProviderId, String)>, SsoFailure>>;
}

/// No SAML providers (the default).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSamlProviders;

impl SamlProviders for NoSamlProviders {
    fn get<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _id: &'a ProviderId,
    ) -> BoxFuture<'a, Result<Option<SamlProvider>, SsoFailure>> {
        Box::pin(async { Ok(None) })
    }
    fn list<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<Vec<(ProviderId, String)>, SsoFailure>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The logout operations of `SsoPort`, installed by `services/sso_logout.rs`.
pub trait SsoLogoutHandler: Send + Sync + 'static {
    /// `sso_backchannel_logout`.
    fn backchannel_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        logout_token: &'a str,
    ) -> BoxFuture<'a, Result<(), SsoFailure>>;

    /// `sso_slo_get` and `sso_slo_post`.
    fn single_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a SloParams,
    ) -> BoxFuture<'a, Result<SloOutcome, SsoFailure>>;
}

/// The ports the host reads, grouped so the service holds one value.
#[derive(Clone)]
pub struct Collaborators {
    /// The directory and the session; `None` until a module provides one.
    pub identity: Option<Arc<dyn SsoIdentityPort>>,
    /// The client-secret reader; `None` until a module provides one.
    pub secrets: Option<Arc<dyn SsoSecrets>>,
    /// The audit sink.
    pub audit: Arc<dyn SsoAudit>,
    /// The SAML providers.
    pub saml: Arc<dyn SamlProviders>,
}
