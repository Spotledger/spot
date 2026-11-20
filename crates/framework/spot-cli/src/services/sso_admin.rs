//! Single sign-on administration (WP-IAM-05.T16): [`SsoAdmin`] is the production
//! [`SsoAdminPort`] behind `/api/v1/admin/sso/*` (provider create, list, get, replace, delete,
//! test; the tenant policy get and put).
//!
//! # Rules
//!
//! - **Who.** The caller is resolved by the site's sign-in service (a bearer credential, never an
//!   API key without an owner, never a session that has not chosen its password) and must be an
//!   **enabled Tenant Administrator** (read from the assignments on every call, not cached). Any
//!   failure to decide is a refusal.
//! - **Step-up.** [`SsoAdminPort::step_up_current`] is "the session was created within the last 15
//!   minutes" (no step-up marker exists yet); the route layer asks it before every write.
//! - **Tenant.** Every store is the site's own; a provider of another tenant is exactly as missing
//!   (`404 sso.provider_not_found`).
//! - **The policy change is reviewed.** A change that would lock the tenant out (switching to
//!   `sso_required` without two usable break-glass administrators and one healthy provider) is
//!   refused with `sso.would_lock_out` and the problem codes, by `signin_policy::review_change`.
//!   The change is a compare-and-set on the policy version (`If-Match`); the loser of two
//!   concurrent changes gets `412`.
//! - **No secret.** A provider holds only the *name* of its client secret; a view says whether the
//!   name is configured, never its value. Nothing logged names a value.
//! - **Audit.** Every outcome, success or refusal, is one structured line on
//!   `spot::audit::identity` (operation, actor, outcome, code); a policy change also writes the
//!   `identity_sso_policy_changed` event with the new mode and the break-glass count.
//!
//! # What this takes and does not take
//!
//! OpenID Connect providers are created from the contract's input: the id is a slug of the name,
//! the redirect URI is `<base_url>/api/v1/auth/sso/<id>/callback`, the discovery URL is derived
//! from the issuer, the algorithms are RS256 and ES256 and the clock skew 60 s. The provider store
//! has no display name or version, so `name` is the id and `If-Match` is a digest of the stored
//! configuration. A provider holds one audience, so the input's `audience` array must have exactly
//! one entry. The input's `jit` is a tenant policy matter in `spot-sso` (rules in the policy), not a
//! provider one: only `off` is accepted.
//!
//! # SAML providers (WP-IAM-05.T17)
//!
//! A SAML provider is created and replaced from the IdP's **metadata document**: the input names its
//! `saml.metadata_url` (HTTPS only), the host fetches it through the egress-policed fetcher
//! (bounded size and time, no redirect), and `spot_sso::saml::parse_idp_metadata` validates it (an
//! `EntityDescriptor` with an entity id, a single-sign-on endpoint and a signing certificate, not
//! expired). A document that does not validate is `422 sso.provider_invalid` naming the metadata
//! error code and never the document. The administrator who passed the step-up chose the URL, and
//! the metadata arrived over TLS from it, so an unsigned document is accepted (the certificates it
//! pins are then the ones the IdP served: trust on first use by an authorised administrator; the
//! contract has no field to pin a metadata certificate). If `saml.entity_id` is also given it must
//! equal the document's. The service provider side is derived: entity id
//! `<base_url>/api/v1/auth/sso/<id>/saml-metadata`, ACS `.../acs`, logout `.../slo`, an assertion
//! signature required, a signing key only if the operator configures one (never in this API). The
//! version (`If-Match`) of a SAML provider is its stored version. Without a configured fetcher a
//! SAML create is `503 server.unavailable`.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value as Json, json};
use spot_core::{Audience, Clock};
use spot_http::api::ports::sso::{AdminPage, AdminReply, SsoAdminPort};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_sso::saml::{IdpMetadata, SamlClaimsMap, SpConfig, parse_idp_metadata};
use spot_sso::{
    ClaimsMap, ClientAuth, Jwa, MetadataTrust, ProviderConfig, ProviderId, ProviderStore,
    ProviderStoreError, SignInMode, SignInPolicy, SsoError, UserOptions,
};
use spot_storage_contract::{Catalog, RecordId, RowLock};

use crate::live::SiteLive;
use crate::audit::PathClass;
use crate::services::scim_store::{at, nm};
use crate::services::signin_policy::review_change;
use crate::services::sso::{FetchError, Fetcher, SamlProvider};
use crate::services::sso_identity::AUDIT;
use crate::services::sso_identity::directory::enabled_admins;
use crate::services::sso_identity::port::SsoIdentityService;
use crate::services::sso_policy::{PgPolicyStore, PolicyStoreError};
use crate::services::sso_saml_store::{PgSamlStore, SamlStoreError};

/// A session older than this is not a recent step-up.
const STEP_UP_WINDOW_SECS: i64 = 900;
/// The scopes a provider gets when the input names none.
const DEFAULT_SCOPES: [&str; 3] = ["openid", "email", "profile"];

/// The egress-policed fetcher the SAML metadata is read with, filled once every module has
/// registered (the fetcher belongs to the sign-in host, which registers later). Empty: a SAML create
/// or replace is `503 server.unavailable`.
pub type FetcherSlot = std::sync::OnceLock<Arc<dyn Fetcher>>;

/// What the administration is built from.
pub struct Parts {
    /// The site (the tenant).
    pub site: String,
    /// The running site: sign-in service and engine.
    pub live: Arc<SiteLive>,
    /// The platform catalog.
    pub catalog: Arc<Catalog>,
    /// The injected clock.
    pub clock: Arc<dyn Clock>,
    /// The tenant's policy.
    pub policy: Arc<PgPolicyStore>,
    /// The OpenID Connect provider store.
    pub oidc: Arc<dyn ProviderStore>,
    /// The SAML provider store.
    pub saml: Arc<PgSamlStore>,
    /// The identity service (the policy review reads the directory through it).
    pub identity: Arc<SsoIdentityService>,
    /// The site's public origin, for redirect URIs.
    pub base_url: String,
    /// The names of the configured client secrets (a view says whether one is set).
    pub secret_names: BTreeSet<String>,
    /// Where SAML metadata is fetched from (see [`FetcherSlot`]).
    pub fetcher: Arc<FetcherSlot>,
}

/// The administration port of one site.
pub struct SsoAdmin {
    parts: Parts,
}

impl SsoAdmin {
    /// The port over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self { parts }
    }
}

// ---------------------------------------------------------------------------------------------
// Pure mapping (tested without a database)
// ---------------------------------------------------------------------------------------------

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::SsoProviderInvalid, message)
}

fn bad_request(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// A provider id from a display name: lower-case letters, digits and `-`, at most 64 characters.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    let out: String = out.chars().take(64).collect();
    if out.is_empty() {
        "provider".to_owned()
    } else {
        out
    }
}

fn text_of<'a>(map: &'a Map<String, Json>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Json::as_str)
}

fn audience_of(body: &Map<String, Json>) -> Result<Audience, ApiError> {
    let list = body
        .get("audience")
        .and_then(Json::as_array)
        .ok_or_else(|| bad_request("`audience` is required."))?;
    let mut words: Vec<&str> = list.iter().filter_map(Json::as_str).collect();
    words.sort_unstable();
    words.dedup();
    match words.as_slice() {
        ["staff"] => Ok(Audience::Staff),
        ["partner"] => Ok(Audience::Partner),
        _ => Err(invalid(
            "A provider signs in one audience: give exactly one of `staff` or `partner`.",
        )),
    }
}

/// The OpenID Connect configuration the input describes, for provider `id` on `base_url`.
///
/// # Errors
///
/// `sso.provider_invalid` / `api.invalid_request` for an input this API does not take.
pub fn oidc_from_input(
    id: &ProviderId,
    body: &Map<String, Json>,
    base_url: &str,
) -> Result<ProviderConfig, ApiError> {
    if base_url.is_empty() {
        return Err(ApiError::from_code(Code::ServerUnavailable));
    }
    if text_of(body, "kind") != Some("oidc") {
        return Err(bad_request(
            "SAML providers are set up from the IdP's signed metadata, which this API does not take yet.",
        ));
    }
    if let Some(jit) = text_of(body, "jit")
        && jit != "off"
    {
        return Err(invalid(
            "Just-in-time creation is a rule of the tenant policy, not a provider setting.",
        ));
    }
    let audience = audience_of(body)?;
    let oidc = body
        .get("oidc")
        .and_then(Json::as_object)
        .ok_or_else(|| bad_request("`oidc` is required for an OpenID Connect provider."))?;
    let issuer =
        text_of(oidc, "issuer").ok_or_else(|| bad_request("`oidc.issuer` is required."))?;
    let client_id =
        text_of(oidc, "client_id").ok_or_else(|| bad_request("`oidc.client_id` is required."))?;
    let secret = text_of(oidc, "client_secret_name")
        .filter(|n| crate::services::sso_secrets::name_ok(n))
        .ok_or_else(|| {
            bad_request("`oidc.client_secret_name` is required and must be a secret name.")
        })?;
    let mut scopes: Vec<String> = match oidc.get("scopes").and_then(Json::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_owned)
            .collect(),
        None => DEFAULT_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
    };
    if !scopes.iter().any(|s| s == "openid") {
        scopes.insert(0, "openid".to_owned());
    }
    let mut claims = ClaimsMap::default();
    if let Some(map) = oidc.get("claims_map").and_then(Json::as_object) {
        for (key, value) in map {
            let value = value
                .as_str()
                .ok_or_else(|| bad_request("`oidc.claims_map` values are claim names."))?
                .to_owned();
            match key.as_str() {
                "subject" => claims.subject = value,
                "email" => claims.email = value,
                "name" => claims.name = value,
                "groups" => claims.groups = value,
                _ => {
                    return Err(bad_request(
                        "`oidc.claims_map` has a key this API does not take.",
                    ));
                }
            }
        }
    }
    Ok(ProviderConfig {
        id: id.clone(),
        issuer: issuer.to_owned(),
        client_id: client_id.to_owned(),
        auth: ClientAuth::ClientSecretRef {
            secret_ref: secret.to_owned(),
        },
        scopes,
        redirect_uri: format!("{base_url}/api/v1/auth/sso/{}/callback", id.as_str()),
        discovery_url: format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        ),
        allowed_algs: vec![Jwa::Rs256, Jwa::Es256],
        clock_skew_seconds: 60,
        max_logout_token_lifetime_seconds: 120,
        claims_map: claims,
        require_email_verified: true,
        acr_values: Vec::new(),
        audience,
        user_options: UserOptions::default(),
    })
}

/// The metadata trust the administration applies: the document came over TLS from the address an
/// authorised administrator chose, with no certificate pinned in advance (see the module docs).
#[must_use]
pub fn metadata_trust() -> MetadataTrust {
    MetadataTrust {
        pinned_certificate: None,
        trust_unsigned: true,
    }
}

/// The API error of a metadata problem: `422 sso.provider_invalid` with the SAML error code as a
/// detail, never the document.
fn metadata_error(error: &SsoError) -> ApiError {
    invalid("The identity provider's metadata is not valid.")
        .detail("code", Json::String(error.code().to_owned()))
}

/// Fetches the IdP metadata at `url` for `tenant` through `fetcher` and validates it at `now`.
///
/// # Errors
///
/// `server.unavailable` when the address cannot be reached in time; `sso.provider_invalid` when the
/// egress policy refuses the address, the answer is not `2xx`, or the document does not validate
/// (the metadata error code is the detail, the document is never quoted).
pub async fn fetch_metadata(
    fetcher: &dyn Fetcher,
    tenant: &str,
    url: &str,
    now: i64,
) -> Result<IdpMetadata, ApiError> {
    let bytes = fetcher
        .get(tenant, url)
        .await
        .map_err(|error| match error {
            FetchError::Timeout | FetchError::Unavailable => {
                ApiError::from_code(Code::ServerUnavailable)
            }
            _ => invalid("The metadata could not be fetched from that address."),
        })?;
    parse_idp_metadata(&bytes, &metadata_trust(), now).map_err(|e| metadata_error(&e))
}

/// The HTTPS address of the IdP metadata the SAML input names.
///
/// # Errors
///
/// `api.invalid_request` / `sso.provider_invalid` for an input that is not a SAML provider with an
/// HTTPS metadata address, a `jit` other than `off`, or no single audience.
pub fn saml_metadata_url(body: &Map<String, Json>) -> Result<String, ApiError> {
    if text_of(body, "kind") != Some("saml") {
        return Err(bad_request("`kind` is not `saml`."));
    }
    if let Some(jit) = text_of(body, "jit")
        && jit != "off"
    {
        return Err(invalid(
            "Just-in-time creation is a rule of the tenant policy, not a provider setting.",
        ));
    }
    audience_of(body)?;
    let saml = body
        .get("saml")
        .and_then(Json::as_object)
        .ok_or_else(|| bad_request("`saml` is required for a SAML provider."))?;
    let url = text_of(saml, "metadata_url")
        .ok_or_else(|| bad_request("`saml.metadata_url` is required."))?;
    let parsed = url::Url::parse(url)
        .map_err(|_| invalid("`saml.metadata_url` is not an HTTPS address."))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid("`saml.metadata_url` is not an HTTPS address."));
    }
    Ok(url.to_owned())
}

/// The SAML provider the input and the validated metadata describe, for provider `id` on
/// `base_url`.
///
/// # Errors
///
/// `sso.provider_invalid` when `saml.entity_id` disagrees with the metadata, the metadata has no
/// usable endpoint, or the service provider settings do not validate; `server.unavailable` with no
/// public origin configured (never a guess).
pub fn saml_from_metadata(
    id: &ProviderId,
    body: &Map<String, Json>,
    base_url: &str,
    metadata: &IdpMetadata,
) -> Result<SamlProvider, ApiError> {
    if base_url.is_empty() {
        return Err(ApiError::from_code(Code::ServerUnavailable));
    }
    saml_metadata_url(body)?;
    let audience = audience_of(body)?;
    if let Some(entity) = body
        .get("saml")
        .and_then(Json::as_object)
        .and_then(|saml| text_of(saml, "entity_id"))
        && entity != metadata.entity_id
    {
        return Err(invalid(
            "`saml.entity_id` is not the entity id the metadata publishes.",
        ));
    }
    let sp = SpConfig {
        entity_id: format!("{base_url}/api/v1/auth/sso/{}/saml-metadata", id.as_str()),
        acs_url: format!("{base_url}/api/v1/auth/sso/{}/acs", id.as_str()),
        slo_url: Some(format!("{base_url}/api/v1/auth/sso/{}/slo", id.as_str())),
        ..SpConfig::default()
    };
    sp.validate()
        .map_err(|_| invalid("The service provider settings are not valid."))?;
    let idp = metadata
        .into_config(id.clone(), SamlClaimsMap::default(), Vec::new())
        .map_err(|e| metadata_error(&e))?;
    let name = text_of(body, "name").unwrap_or(id.as_str()).to_owned();
    Ok(SamlProvider {
        sp,
        idp,
        name,
        audience,
    })
}

/// The version text (the `ETag`) of a stored OpenID Connect configuration: a stable 64-bit
/// FNV-1a digest of its JSON (an `ETag` is a change detector, not a secret).
#[must_use]
pub fn oidc_etag(config: &ProviderConfig) -> String {
    let text = serde_json::to_string(config).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // The contract's `ETag` is a quoted positive integer of at most 18 digits (the routes refuse
    // anything else): the digest, cut to 59 bits and never zero. The caller quotes it.
    ((hash & ((1 << 59) - 1)).max(1)).to_string()
}

/// An `ETag` as the contract writes it: the decimal version in double quotes.
fn quoted(etag: &str) -> String {
    format!("\"{etag}\"")
}

fn view_oidc(config: &ProviderConfig, secret_set: bool) -> Json {
    json!({
        "id": config.id.as_str(),
        "kind": "oidc",
        "name": config.id.as_str(),
        "audience": [config.audience.as_str()],
        "enabled": true,
        "issuer": config.issuer,
        "client_id": config.client_id,
        "client_secret_set": secret_set,
        "scopes": config.scopes,
        "metadata_url": Json::Null,
        "entity_id": Json::Null,
        "jit": "off",
    })
}

fn view_saml(id: &ProviderId, provider: &crate::services::sso::SamlProvider) -> Json {
    json!({
        "id": id.as_str(),
        "kind": "saml",
        "name": provider.name,
        "audience": [provider.audience.as_str()],
        "enabled": true,
        "issuer": Json::Null,
        "client_id": Json::Null,
        "client_secret_set": false,
        "scopes": [],
        "metadata_url": Json::Null,
        "entity_id": provider.idp.entity_id,
        "jit": "off",
    })
}

fn mode_word(mode: SignInMode) -> &'static str {
    match mode {
        SignInMode::Local => "local",
        SignInMode::SsoOptional => "sso_optional",
        SignInMode::SsoRequired => "sso_required",
    }
}

/// The version the API shows: a tenant with no stored policy is version 1 (the contract's minimum).
fn shown_version(stored: i64) -> i64 {
    stored.max(1)
}

fn policy_body(policy: &SignInPolicy, stored: i64) -> Json {
    json!({
        "mode": mode_word(policy.mode),
        "break_glass_users": policy.break_glass.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "version": shown_version(stored),
    })
}

/// The policy a `SsoPolicyInput` describes, built on the stored one (providers are the configured
/// ones, just-in-time rules and the second-factor rule are kept).
///
/// # Errors
///
/// `api.invalid_request` for a mode or a user id that is not one.
pub fn policy_from_input(
    old: &SignInPolicy,
    body: &Map<String, Json>,
    providers: Vec<ProviderId>,
) -> Result<SignInPolicy, ApiError> {
    let mode = match text_of(body, "mode") {
        Some("local") => SignInMode::Local,
        Some("sso_optional") => SignInMode::SsoOptional,
        Some("sso_required") => SignInMode::SsoRequired,
        _ => {
            return Err(bad_request(
                "`mode` is local, sso_optional or sso_required.",
            ));
        }
    };
    let users = body
        .get("break_glass_users")
        .and_then(Json::as_array)
        .ok_or_else(|| bad_request("`break_glass_users` is required."))?;
    let mut break_glass = Vec::new();
    for user in users {
        let id = user
            .as_str()
            .and_then(spot_http::api::codec::parse_record_id)
            .ok_or_else(|| bad_request("`break_glass_users` holds user ids."))?;
        if !break_glass.contains(&id) {
            break_glass.push(id);
        }
    }
    Ok(SignInPolicy {
        mode,
        providers,
        jit: old.jit.clone(),
        break_glass,
        require_second_factor_for_break_glass: old.require_second_factor_for_break_glass,
    })
}

fn version_of(if_match: &str) -> Option<i64> {
    if_match
        .trim_matches(|c| c == '"' || c == 'W' || c == '/')
        .parse()
        .ok()
}

// ---------------------------------------------------------------------------------------------
// The port
// ---------------------------------------------------------------------------------------------

impl SsoAdmin {
    fn unavailable() -> ApiError {
        ApiError::from_code(Code::ServerUnavailable)
    }

    fn audit(&self, caller: Option<RecordId>, op: &str, outcome: &str, code: &str) {
        tracing::info!(
            target: AUDIT,
            site = %self.parts.site,
            actor = caller.map(|c| c.to_string()).as_deref().unwrap_or(""),
            op,
            outcome,
            code,
            "single sign-on administration"
        );
    }

    /// The caller, who must be an enabled Tenant Administrator.
    async fn caller(&self, ctx: &RequestCtx, op: &str) -> Result<RecordId, ApiError> {
        let refuse = |this: &Self, caller: Option<RecordId>| {
            this.audit(caller, op, "refused", "permission.denied");
            ApiError::from_code(Code::PermissionDenied)
        };
        if ctx.site.as_str() != self.parts.site {
            return Err(refuse(self, None));
        }
        let auth = self.parts.live.auth().ok_or_else(Self::unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(refuse(self, None));
        }
        let engine = self.parts.live.engine().ok_or_else(Self::unavailable)?;
        let options = spot_storage_contract::TxOptions::new(
            nm("sso_admin_caller").map_err(|_| Self::unavailable())?,
            spot_storage_contract::Isolation::ReadCommitted,
        )
        .read_only();
        let mut tx = spot_storage_contract::StorageBackend::begin(
            engine.backend(),
            Arc::clone(&self.parts.catalog),
            options,
        )
        .await
        .map_err(|_| Self::unavailable())?;
        let admins = enabled_admins(&mut tx).await;
        let _ = spot_storage_contract::Transaction::rollback(tx).await;
        match admins {
            Ok(admins) if admins.contains(&who.user_id) => Ok(who.user_id),
            Ok(_) => Err(refuse(self, Some(who.user_id))),
            Err(_) => Err(Self::unavailable()),
        }
    }

    async fn all_providers(&self) -> Result<Vec<(ProviderId, Json, String)>, ApiError> {
        let mut out = Vec::new();
        let listing = self
            .parts
            .oidc
            .list()
            .await
            .map_err(|_| Self::unavailable())?;
        for config in &listing.providers {
            let set = self.parts.secret_names.contains(config.auth.reference());
            out.push((config.id.clone(), view_oidc(config, set), oidc_etag(config)));
        }
        for saml in self
            .parts
            .saml
            .all()
            .await
            .map_err(|_| Self::unavailable())?
        {
            let etag = saml.version.to_string();
            out.push((saml.id.clone(), view_saml(&saml.id, &saml.provider), etag));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn one(&self, id: &str) -> Result<(ProviderId, Json, String), ApiError> {
        let id = ProviderId::new(id);
        self.all_providers()
            .await?
            .into_iter()
            .find(|(found, _, _)| *found == id)
            .ok_or_else(|| ApiError::from_code(Code::SsoProviderNotFound))
    }

    fn store_error(error: &ProviderStoreError) -> ApiError {
        match error {
            ProviderStoreError::Invalid(_) => invalid("The provider settings are not valid."),
            _ => Self::unavailable(),
        }
    }

    fn saml_store_error(error: SamlStoreError) -> ApiError {
        match error {
            SamlStoreError::Invalid => invalid("The provider settings are not valid."),
            SamlStoreError::Exists => invalid("A provider with that id already exists."),
            SamlStoreError::Stale => ApiError::from_code(Code::DocumentStaleVersion),
            SamlStoreError::Unavailable => Self::unavailable(),
        }
    }

    /// Fetches and validates the IdP metadata at `url`.
    async fn load_metadata(&self, url: &str) -> Result<IdpMetadata, ApiError> {
        let fetcher = self.parts.fetcher.get().ok_or_else(Self::unavailable)?;
        let now = self.parts.clock.now_utc().unix_timestamp();
        fetch_metadata(fetcher.as_ref(), &self.parts.site, url, now).await
    }

    /// One SAML create or replace: the input is checked, the metadata fetched and validated, the
    /// provider stored (`expected` is the version a replace names), every outcome audited.
    async fn write_saml(
        &self,
        caller: RecordId,
        op: &str,
        id: &ProviderId,
        body: &Map<String, Json>,
        expected: Option<i64>,
    ) -> Result<AdminReply, ApiError> {
        let result = async {
            let url = saml_metadata_url(body)?;
            let metadata = self.load_metadata(&url).await?;
            let provider = saml_from_metadata(id, body, &self.parts.base_url, &metadata)?;
            let version = self
                .parts
                .saml
                .put(id, &provider, expected)
                .await
                .map_err(Self::saml_store_error)?;
            Ok::<_, ApiError>((provider, version))
        }
        .await;
        match result {
            Ok((provider, version)) => {
                self.audit(Some(caller), op, "ok", "");
                Ok(AdminReply {
                    data: view_saml(id, &provider),
                    etag: Some(quoted(&version.to_string())),
                    next_cursor: None,
                })
            }
            Err(error) => {
                self.audit(Some(caller), op, "refused", error.code.as_str());
                Err(error)
            }
        }
    }

    async fn unique_id(&self, name: &str) -> Result<ProviderId, ApiError> {
        let base = slug(name);
        let taken: BTreeSet<ProviderId> = self
            .all_providers()
            .await?
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        for n in 1..=50 {
            let candidate = if n == 1 {
                base.clone()
            } else {
                let suffix = format!("-{n}");
                let stem: String = base.chars().take(64 - suffix.len()).collect();
                format!("{stem}{suffix}")
            };
            let id = ProviderId::new(candidate);
            if !taken.contains(&id) {
                return Ok(id);
            }
        }
        Err(invalid("Too many providers share that name."))
    }
}

impl SsoAdminPort for SsoAdmin {
    fn step_up_current<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.parts.site {
                return Ok(false);
            }
            let auth = self.parts.live.auth().ok_or_else(Self::unavailable)?;
            let who = auth.authenticate_on(ctx, PathClass::Api).await?;
            let Some(session) = who
                .session_id
                .as_deref()
                .and_then(spot_http::api::codec::parse_record_id)
            else {
                return Ok(false);
            };
            let engine = self.parts.live.engine().ok_or_else(Self::unavailable)?;
            let options = spot_storage_contract::TxOptions::new(
                nm("sso_admin_step_up").map_err(|_| Self::unavailable())?,
                spot_storage_contract::Isolation::ReadCommitted,
            )
            .read_only();
            let mut tx = spot_storage_contract::StorageBackend::begin(
                engine.backend(),
                Arc::clone(&self.parts.catalog),
                options,
            )
            .await
            .map_err(|_| Self::unavailable())?;
            let entity =
                nm(spot_platform_app::doctype::SESSION).map_err(|_| Self::unavailable())?;
            let loaded = spot_storage_contract::DocumentStore::load(
                &mut tx,
                &entity,
                session,
                RowLock::None,
            )
            .await;
            let _ = spot_storage_contract::Transaction::rollback(tx).await;
            let created = loaded.map_err(|_| Self::unavailable())?.and_then(|d| {
                nm("created_at")
                    .ok()
                    .and_then(|n| d.data.fields.get(&n).and_then(at))
            });
            let now = self.parts.clock.now_utc();
            Ok(created.is_some_and(|c| (now - c).whole_seconds() <= STEP_UP_WINDOW_SECS))
        })
    }

    fn list_providers<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: &'a AdminPage,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_list").await?;
            let all = self.all_providers().await?;
            let after = page.cursor.as_deref();
            let mut items: Vec<(ProviderId, Json)> = all
                .into_iter()
                .filter(|(id, _, _)| after.is_none_or(|a| id.as_str() > a))
                .map(|(id, view, _)| (id, view))
                .collect();
            let limit = page.limit as usize;
            let next = (items.len() > limit).then(|| items[limit - 1].0.as_str().to_owned());
            items.truncate(limit);
            self.audit(Some(caller), "sso_admin_provider_list", "ok", "");
            Ok(AdminReply {
                data: Json::Array(items.into_iter().map(|(_, v)| v).collect()),
                etag: None,
                next_cursor: Some(next),
            })
        })
    }

    fn create_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        _idempotency_key: &'a str,
        body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_create").await?;
            let name = text_of(body, "name").unwrap_or("provider");
            let id = self.unique_id(name).await?;
            if text_of(body, "kind") == Some("saml") {
                return self
                    .write_saml(caller, "sso_admin_provider_create", &id, body, None)
                    .await;
            }
            let config = oidc_from_input(&id, body, &self.parts.base_url)?;
            if let Err(error) = self.parts.oidc.put(&config).await {
                let error = Self::store_error(&error);
                self.audit(
                    Some(caller),
                    "sso_admin_provider_create",
                    "refused",
                    "sso.provider_invalid",
                );
                return Err(error);
            }
            let set = self.parts.secret_names.contains(config.auth.reference());
            self.audit(Some(caller), "sso_admin_provider_create", "ok", "");
            Ok(AdminReply {
                data: view_oidc(&config, set),
                etag: Some(quoted(&oidc_etag(&config))),
                next_cursor: None,
            })
        })
    }

    fn get_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_get").await?;
            let (_, view, etag) = self.one(id).await?;
            self.audit(Some(caller), "sso_admin_provider_get", "ok", "");
            Ok(AdminReply {
                data: view,
                etag: Some(quoted(&etag)),
                next_cursor: None,
            })
        })
    }

    fn put_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        if_match: &'a str,
        body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_put").await?;
            let (pid, view, etag) = self.one(id).await?;
            if version_text(if_match) != etag {
                self.audit(
                    Some(caller),
                    "sso_admin_provider_put",
                    "refused",
                    "document.stale_version",
                );
                return Err(ApiError::from_code(Code::DocumentStaleVersion));
            }
            let stored_is_saml = view.get("kind").and_then(Json::as_str) == Some("saml");
            if stored_is_saml != (text_of(body, "kind") == Some("saml")) {
                // A provider keeps its kind: an OpenID Connect provider is not replaced by a SAML
                // one (or the reverse); create a new one and delete the old.
                self.audit(
                    Some(caller),
                    "sso_admin_provider_put",
                    "refused",
                    "api.invalid_request",
                );
                return Err(bad_request("A provider cannot change its `kind`."));
            }
            if stored_is_saml {
                let version = etag.parse::<i64>().map_err(|_| Self::unavailable())?;
                return self
                    .write_saml(caller, "sso_admin_provider_put", &pid, body, Some(version))
                    .await;
            }
            let config = oidc_from_input(&pid, body, &self.parts.base_url)?;
            if let Err(error) = self.parts.oidc.put(&config).await {
                self.audit(
                    Some(caller),
                    "sso_admin_provider_put",
                    "refused",
                    "sso.provider_invalid",
                );
                return Err(Self::store_error(&error));
            }
            let set = self.parts.secret_names.contains(config.auth.reference());
            self.audit(Some(caller), "sso_admin_provider_put", "ok", "");
            Ok(AdminReply {
                data: view_oidc(&config, set),
                etag: Some(quoted(&oidc_etag(&config))),
                next_cursor: None,
            })
        })
    }

    fn delete_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        if_match: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_delete").await?;
            let (pid, view, etag) = self.one(id).await?;
            if version_text(if_match) != etag {
                self.audit(
                    Some(caller),
                    "sso_admin_provider_delete",
                    "refused",
                    "document.stale_version",
                );
                return Err(ApiError::from_code(Code::DocumentStaleVersion));
            }
            let is_saml = view.get("kind").and_then(Json::as_str) == Some("saml");
            let result = if is_saml {
                let version = etag.parse::<i64>().map_err(|_| Self::unavailable())?;
                self.parts
                    .saml
                    .delete(&pid, version)
                    .await
                    .map_err(|e| match e {
                        SamlStoreError::Stale => ApiError::from_code(Code::DocumentStaleVersion),
                        _ => Self::unavailable(),
                    })
            } else {
                self.parts
                    .oidc
                    .delete(&pid)
                    .await
                    .map(|_| ())
                    .map_err(|_| Self::unavailable())
            };
            let outcome = if result.is_ok() { "ok" } else { "refused" };
            self.audit(Some(caller), "sso_admin_provider_delete", outcome, "");
            result
        })
    }

    fn test_provider<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_provider_test").await?;
            let (pid, view, _) = self.one(id).await?;
            // Offline checks only: the settings validate (they were served, so they do), and the
            // client secret's name is configured. Reaching the IdP is not tried from here.
            let mut checks = vec![json!({"name": "settings", "ok": true, "code": Json::Null})];
            if view.get("kind").and_then(Json::as_str) == Some("oidc") {
                let set = view
                    .get("client_secret_set")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                checks.push(json!({
                    "name": "client_secret",
                    "ok": set,
                    "code": if set { Json::Null } else { json!("sso.secret_ref_invalid") },
                }));
            }
            let ok = checks.iter().all(|c| c["ok"] == Json::Bool(true));
            self.audit(
                Some(caller),
                "sso_admin_provider_test",
                if ok { "ok" } else { "failed" },
                "",
            );
            let _ = pid;
            Ok(AdminReply {
                data: json!({"ok": ok, "checks": checks}),
                etag: None,
                next_cursor: None,
            })
        })
    }

    fn get_policy<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_policy_get").await?;
            let (policy, version) = self
                .parts
                .policy
                .read()
                .await
                .map_err(|_| Self::unavailable())?;
            self.audit(Some(caller), "sso_admin_policy_get", "ok", "");
            Ok(AdminReply {
                data: policy_body(&policy, version),
                etag: Some(quoted(&shown_version(version).to_string())),
                next_cursor: None,
            })
        })
    }

    fn put_policy<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        if_match: &'a str,
        body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            let caller = self.caller(ctx, "sso_admin_policy_put").await?;
            let (old, stored) = self
                .parts
                .policy
                .read()
                .await
                .map_err(|_| Self::unavailable())?;
            if version_of(if_match) != Some(shown_version(stored)) {
                self.audit(
                    Some(caller),
                    "sso_admin_policy_put",
                    "refused",
                    "document.stale_version",
                );
                return Err(ApiError::from_code(Code::DocumentStaleVersion));
            }
            let providers = self
                .parts
                .identity
                .provider_ids()
                .await
                .map_err(|_| Self::unavailable())?;
            let new = policy_from_input(&old, body, providers)?;
            let view = self
                .parts
                .identity
                .review_view(&new)
                .await
                .map_err(|_| Self::unavailable())?;
            let record = match review_change(&old, &new, &view) {
                Ok(record) => record,
                Err(codes) => {
                    let first = codes.first().copied().unwrap_or("sso.policy_lockout");
                    self.audit(Some(caller), "sso_admin_policy_put", "refused", first);
                    let mut error = ApiError::from_code(Code::SsoWouldLockOut);
                    error = error.detail(
                        "codes",
                        Json::Array(codes.iter().map(|c| Json::from(*c)).collect()),
                    );
                    return Err(error);
                }
            };
            let version = match self.parts.policy.write(&new, stored).await {
                Ok(version) => version,
                Err(PolicyStoreError::Stale) => {
                    self.audit(
                        Some(caller),
                        "sso_admin_policy_put",
                        "refused",
                        "document.stale_version",
                    );
                    return Err(ApiError::from_code(Code::DocumentStaleVersion));
                }
                Err(PolicyStoreError::Unavailable) => return Err(Self::unavailable()),
            };
            tracing::info!(
                target: AUDIT,
                site = %self.parts.site,
                actor = %caller,
                op = "sso_admin_policy_put",
                before = record.before,
                after = record.after,
                break_glass_before = record.break_glass.0,
                break_glass_after = record.break_glass.1,
                outcome = "ok",
                "single sign-on policy changed"
            );
            Ok(AdminReply {
                data: policy_body(&new, version),
                etag: Some(quoted(&shown_version(version).to_string())),
                next_cursor: None,
            })
        })
    }
}

/// The `If-Match` text without quotes or a weak marker.
fn version_text(if_match: &str) -> String {
    if_match
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn body(text: &str) -> Map<String, Json> {
        serde_json::from_str::<Json>(text)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    const OIDC: &str = r#"{"kind":"oidc","name":"Acme Okta","audience":["staff"],
        "oidc":{"issuer":"https://acme.okta.example/","client_id":"abc","client_secret_name":"okta-prod"}}"#;

    #[test]
    fn a_name_becomes_a_url_safe_id() {
        assert_eq!(slug("Acme Okta (prod)"), "acme-okta-prod");
        assert_eq!(slug("  --  "), "provider");
        assert_eq!(slug(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn an_oidc_input_becomes_a_complete_configuration() {
        let id = ProviderId::new("acme-okta");
        let config = oidc_from_input(&id, &body(OIDC), "https://erp.acme.example").unwrap();
        assert_eq!(config.audience, Audience::Staff);
        assert_eq!(
            config.redirect_uri,
            "https://erp.acme.example/api/v1/auth/sso/acme-okta/callback"
        );
        assert_eq!(
            config.discovery_url,
            "https://acme.okta.example/.well-known/openid-configuration"
        );
        assert!(config.scopes.iter().any(|s| s == "openid"));
        assert_eq!(config.auth.reference(), "okta-prod");
        assert_eq!(config.allowed_algs, vec![Jwa::Rs256, Jwa::Es256]);
        // The view never carries the secret, only whether its name is set.
        let view = view_oidc(&config, true);
        assert_eq!(view["client_secret_set"], true);
        assert!(!view.to_string().contains("secret_ref"));
    }

    #[test]
    fn inputs_this_api_does_not_take_are_refused() {
        let id = ProviderId::new("x");
        let base = "https://erp.acme.example";
        for (what, text) in [
            ("saml", r#"{"kind":"saml","name":"n","audience":["staff"]}"#),
            (
                "both audiences",
                r#"{"kind":"oidc","name":"n","audience":["staff","partner"],"oidc":{"issuer":"https://i","client_id":"c","client_secret_name":"s"}}"#,
            ),
            (
                "no audience",
                r#"{"kind":"oidc","name":"n","audience":[],"oidc":{"issuer":"https://i","client_id":"c","client_secret_name":"s"}}"#,
            ),
            (
                "jit create",
                r#"{"kind":"oidc","name":"n","audience":["staff"],"jit":"create","oidc":{"issuer":"https://i","client_id":"c","client_secret_name":"s"}}"#,
            ),
            (
                "no secret name",
                r#"{"kind":"oidc","name":"n","audience":["staff"],"oidc":{"issuer":"https://i","client_id":"c"}}"#,
            ),
            (
                "odd claim key",
                r#"{"kind":"oidc","name":"n","audience":["staff"],"oidc":{"issuer":"https://i","client_id":"c","client_secret_name":"s","claims_map":{"role":"r"}}}"#,
            ),
        ] {
            assert!(oidc_from_input(&id, &body(text), base).is_err(), "{what}");
        }
        // No public origin configured: provider creation is unavailable, never a guess.
        assert!(oidc_from_input(&id, &body(OIDC), "").is_err());
    }

    #[test]
    fn a_policy_input_keeps_the_rules_and_names_the_break_glass_users() {
        let admin = RecordId::new_v7();
        let old = SignInPolicy::local();
        let input = body(&format!(
            r#"{{"mode":"sso_required","break_glass_users":["{admin}","{admin}"]}}"#
        ));
        let new = policy_from_input(&old, &input, vec![ProviderId::new("okta")]).unwrap();
        assert_eq!(new.mode, SignInMode::SsoRequired);
        assert_eq!(new.break_glass, vec![admin], "a repeated user counts once");
        assert_eq!(new.providers, vec![ProviderId::new("okta")]);
        for bad in [
            r#"{"mode":"sometimes","break_glass_users":[]}"#,
            r#"{"mode":"local"}"#,
            r#"{"mode":"local","break_glass_users":["not-an-id"]}"#,
        ] {
            assert!(
                policy_from_input(&old, &body(bad), Vec::new()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_policy_version_the_api_shows_is_at_least_one() {
        assert_eq!(shown_version(0), 1);
        assert_eq!(shown_version(7), 7);
        assert_eq!(version_of("\"3\""), Some(3));
        assert_eq!(version_of("W/\"3\""), Some(3));
        assert_eq!(version_of("x"), None);
    }
}
