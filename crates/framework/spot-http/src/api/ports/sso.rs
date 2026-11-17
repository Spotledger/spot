//! The single sign-on seams of the native API (contract 0.17.0, WP-CON-03.T40; ADR-070, ADR-083).
//!
//! Two ports, both object safe with boxed futures like [`crate::api::FilesPort`]:
//!
//! - [`SsoPort`]: the public sign-in group (`sso_provider_list`, `sso_begin`, `sso_callback`,
//!   `sso_acs`, `sso_complete`, `sso_backchannel_logout`, `sso_slo_get`, `sso_slo_post`,
//!   `sso_saml_metadata`). `spot-http` owns routing, cookies, request limits, the rate class
//!   and the error envelope; the sign-in **decision** (state, nonce, PKCE, signature, audience,
//!   clock, the sign-in policy, JIT provisioning, the session) belongs to the host's
//!   implementation (WP-IAM-05.T11) behind this trait, so the HTTP layer can decide nothing
//!   about identity.
//! - [`SsoAdminPort`]: provider and sign-in-policy administration
//!   (`sso_admin_*`) and the step-up check those operations need. The implementation checks
//!   the Tenant Administrator permission (fail closed), the tenant scope, and writes the audit
//!   record of every outcome, refusals included: the layer passes the correlation id in
//!   [`RequestCtx::request_id`] and no actor, tenant or role from the request.
//!
//! A refusal of the public port is a [`SsoFailure`], a closed enum of the contract's `sso.*`
//! codes (plus `unavailable`); the layer maps it to the registry's **generic** message, so the
//! response never says which of state, nonce, signature, audience or clock failed. Nothing in
//! this file logs or prints a secret: [`Binding`], [`Handover`] and [`SignedIn`] redact in
//! `Debug`.

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx, Session};

/// The port key of [`SsoPort`].
pub const KEY: PortKey = PortKey::of::<dyn SsoPort>();

/// The port key of [`SsoAdminPort`] (not in `REGISTERED`: one key per port file; the admin group
/// names it in its own `required_ports`).
pub const ADMIN_KEY: PortKey = PortKey::of::<dyn SsoAdminPort>();

/// Why a public sign-in operation was refused. The layer renders each variant as the registry
/// code of the same name with its default (generic) message; the port records the real reason
/// in the audit record, never here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SsoFailure {
    /// `401 sso.sign_in_failed`: one answer for every cause.
    SignInFailed,
    /// `403 sso.not_allowed`: the policy does not allow this person, role or audience.
    NotAllowed,
    /// `403 sso.required`: single sign-on is required for this user.
    Required,
    /// `400 sso.state_invalid`: binding cookie or state missing, mismatched, expired or used.
    StateInvalid,
    /// `400 sso.request_invalid`: a required parameter is missing or malformed.
    RequestInvalid,
    /// `404 sso.provider_not_found`: unknown, disabled or of another tenant: one answer.
    ProviderNotFound,
    /// `503 sso.provider_unavailable`: the identity provider could not be reached.
    ProviderUnavailable,
    /// `400 sso.logout_rejected`: the logout token or request is not valid; nothing ended.
    LogoutRejected,
    /// `503 server.unavailable`: the port's own store cannot answer now.
    Unavailable,
}

/// The sign-in providers a sign-in page may offer (`SsoProviderPublic`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderPublic {
    /// Opaque id, 1 to 64 characters.
    pub id: String,
    /// Display name, 1 to 100 characters.
    pub name: String,
    /// `oidc` or `saml`.
    pub kind: String,
}

/// The answer of `sso_provider_list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderList {
    /// The providers to offer (empty when the policy is `local`); at most 20.
    pub providers: Vec<ProviderPublic>,
    /// False when the policy is `sso_required`.
    pub password_login: bool,
}

/// What `sso_begin` answers: where to send the browser, and the binding cookie value.
#[derive(Clone, PartialEq, Eq)]
pub struct Begin {
    /// The identity provider URL (`https`, at most 4096 characters). The layer never builds it.
    pub redirect_url: String,
    /// The binding cookie value (a secret; cookie-safe characters only, at most 256).
    pub binding: String,
}

impl std::fmt::Debug for Begin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Begin")
            .field("redirect_url", &"<redacted>")
            .field("binding", &"<redacted>")
            .finish()
    }
}

/// The parameters of `sso_callback`, exactly as received (each at most once; the layer has
/// bounded them) plus the binding cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct CallbackParams {
    /// The authorization code, if any.
    pub code: Option<String>,
    /// The state, if any.
    pub state: Option<String>,
    /// The provider's `error` parameter, if any.
    pub error: Option<String>,
    /// The binding cookie value (always present: the layer refuses without one).
    pub binding: String,
}

impl std::fmt::Debug for CallbackParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackParams")
            .field("code", &"<redacted>")
            .field("state", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// The parameters of `sso_acs`: the SAML response and relay state plus the binding cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct AcsParams {
    /// The base64 `SAMLResponse` (opaque to the layer; bounded).
    pub saml_response: String,
    /// The `RelayState`, if sent.
    pub relay_state: Option<String>,
    /// The binding cookie value (always present).
    pub binding: String,
}

impl std::fmt::Debug for AcsParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcsParams")
            .field("saml_response", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// A completed provider sign-in, ready for the browser to be sent home with the hand-over
/// cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct Handover {
    /// A relative path of this site (starts with one `/`, then not `/` or `\`), at most 2048
    /// characters: the `return_to` recorded by `begin`, or `/`.
    pub location: String,
    /// The one-time hand-over cookie value (a secret; cookie-safe, at most 256 characters).
    pub cookie: String,
}

impl std::fmt::Debug for Handover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handover")
            .field("location", &self.location)
            .field("cookie", &"<redacted>")
            .finish()
    }
}

/// The session `sso_complete` mints (shown once). The token is a secret.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedIn {
    /// The session, in the shape of `auth_login`.
    pub session: Session,
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedIn")
            .field("session", &"<redacted>")
            .finish()
    }
}

/// Which SAML single-logout binding a request arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SloBinding {
    /// HTTP-Redirect (`sso_slo_get`).
    Redirect,
    /// HTTP-POST (`sso_slo_post`).
    Post,
}

/// The parameters of a SAML single-logout call.
#[derive(Clone, PartialEq, Eq)]
pub struct SloParams {
    /// The binding.
    pub binding: SloBinding,
    /// `SAMLRequest`, if sent.
    pub saml_request: Option<String>,
    /// `SAMLResponse`, if sent.
    pub saml_response: Option<String>,
    /// `RelayState`, if sent.
    pub relay_state: Option<String>,
}

impl std::fmt::Debug for SloParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SloParams")
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

/// What a single-logout call answers.
#[derive(Clone, PartialEq, Eq)]
pub enum SloOutcome {
    /// Redirect to the provider's logout response URL (`https`, at most 4096 characters).
    Redirect(String),
    /// An auto-submitting form to the provider (`text/html`, bounded). The page is produced by
    /// the port; the layer fixes the headers (`Content-Type`, no-store, a strict CSP).
    Form(String),
    /// Nothing to send back (a logout response was consumed): `200` with an empty body.
    Done,
}

impl std::fmt::Debug for SloOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SloOutcome::Redirect(_) => "SloOutcome::Redirect(<redacted>)",
            SloOutcome::Form(_) => "SloOutcome::Form(<redacted>)",
            SloOutcome::Done => "SloOutcome::Done",
        })
    }
}

/// The public single sign-on operations. Every method answers for `ctx.site` (the tenant comes
/// from the site) and for no caller identity: none exists yet.
pub trait SsoPort: Send + Sync + 'static {
    /// `sso_provider_list`: the same answer for every caller of the site.
    fn providers<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<ProviderList, SsoFailure>>;

    /// `sso_begin`: starts an attempt; `return_to` is a validated relative path (or `None`).
    fn begin<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        return_to: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Begin, SsoFailure>>;

    /// `sso_callback`: the OIDC redirect target.
    fn callback<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a CallbackParams,
    ) -> BoxFuture<'a, Result<Handover, SsoFailure>>;

    /// `sso_acs`: the SAML assertion consumer.
    fn acs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a AcsParams,
    ) -> BoxFuture<'a, Result<Handover, SsoFailure>>;

    /// `sso_complete`: exchanges the one-time hand-over cookie (the value only) for the session.
    fn complete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, Result<SignedIn, SsoFailure>>;

    /// `sso_backchannel_logout`: validates the logout token and ends the matching sessions.
    fn backchannel_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        logout_token: &'a str,
    ) -> BoxFuture<'a, Result<(), SsoFailure>>;

    /// `sso_slo_get` and `sso_slo_post`: SAML single logout.
    fn single_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a SloParams,
    ) -> BoxFuture<'a, Result<SloOutcome, SsoFailure>>;

    /// `sso_saml_metadata`: the service provider metadata XML (no secret, at most 262144 bytes).
    fn saml_metadata<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
    ) -> BoxFuture<'a, Result<String, SsoFailure>>;
}

/// A successful administration answer: the body's `data` and, where the contract names one, the
/// version for the `ETag` header.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminReply {
    /// The `data` member of the answer.
    pub data: serde_json::Value,
    /// The version the `ETag` header carries (at most 40 characters), if any.
    pub etag: Option<String>,
    /// The `next_cursor` of a page: `Some(None)` is a last page (`null`), `None` is not a page.
    pub next_cursor: Option<Option<String>>,
}

/// A page request of the administration list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminPage {
    /// The opaque cursor of the previous page, if any (1 to 2048 characters).
    pub cursor: Option<String>,
    /// The page size, 1 to 100.
    pub limit: u32,
}

/// The single sign-on administration operations. The port answers for the caller
/// `ctx.credential` names: it checks the Tenant Administrator permission (an error from the
/// permission engine, a missing role snapshot or a timeout is a refusal), scopes every read and
/// write to the caller's tenant (another tenant's id is `404 sso.provider_not_found`), and writes
/// the audit record of every outcome. Errors are [`ApiError`]s of the registry; the layer lets
/// only the codes of the contract through and reports anything else as `http.internal`.
pub trait SsoAdminPort: Send + Sync + 'static {
    /// Whether the caller's session carries a recent step-up marker (ADR-070 section 3).
    /// Called by the layer **before** every write; `false` is `403 auth.step_up_required`
    /// (the contract's status; the brief said 401) and the operation is not called.
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>>;

    /// `sso_admin_provider_list`.
    fn list_providers<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: &'a AdminPage,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_provider_create`: `body` is the bounded `SsoProviderInput` object;
    /// `idempotency_key` is the validated header.
    fn create_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        idempotency_key: &'a str,
        body: &'a serde_json::Map<String, serde_json::Value>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_provider_get`.
    fn get_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_provider_put`.
    fn put_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        if_match: &'a str,
        body: &'a serde_json::Map<String, serde_json::Value>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_provider_delete`.
    fn delete_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        if_match: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>>;

    /// `sso_admin_provider_test`.
    fn test_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_policy_get`.
    fn get_policy<'a>(&'a self, ctx: &'a RequestCtx)
    -> BoxFuture<'a, Result<AdminReply, ApiError>>;

    /// `sso_admin_policy_put`.
    fn put_policy<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        if_match: &'a str,
        body: &'a serde_json::Map<String, serde_json::Value>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>>;
}
