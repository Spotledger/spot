//! The tenant's SAML providers, in PostgreSQL (WP-IAM-05.T16): the provider store of
//! `spot-sso-pg` holds OpenID Connect configurations only, so SAML has its own table,
//! `spot_sso_saml_provider`, in the same shape and under the same rules. Since WP-IAM-05.T18 the SQL
//! lives in `spot-sso-pg` ([`PgSamlRows`]); this file keeps the stored form of a provider
//! ([`StoredSaml`]), the decision whether a row is trustworthy and the host's port adaptation.
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the tenant the store serves; in every predicate of every statement |
//! | `id` | the provider id |
//! | `audience` | `staff` or `partner`, stored with the row and checked against the configuration on read |
//! | `version` | starts at 1 and moves by one on every change: the `ETag` of the administration API and the compare-and-set guard of every update and delete |
//! | `config` | the provider as JSON ([`StoredSaml`]): certificates in hex, the SP signing key only as the *name* of a secret |
//!
//! A row that no longer parses, validates or agrees with its key and audience is **not served**:
//! `list` leaves it out and `get` answers `None` (a provider that cannot be trusted is as good as
//! absent: the sign-in is refused). A database failure is [`SamlStoreError::Unavailable`] with no
//! driver text. Every statement names the tenant; another tenant's row is exactly as missing.
//! [`PgSamlStore`] implements the host's [`SamlProviders`] port, so with it provided a SAML
//! provider can begin and complete a sign-in and answer its metadata.

use serde::{Deserialize, Serialize};
use spot_core::Audience;
use spot_http::api::ports::sso::SsoFailure;
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_sso::saml::{IdpConfig, SamlBinding, SamlClaimsMap, SignaturePolicy, SpConfig};
use spot_sso::{Jwa, ProviderId};
use spot_sso_pg::{MAX_SAML_CONFIG, PgSamlRows, Pool, SamlRowError};

use crate::services::scim_store::{hex, unhex};
use crate::services::sso::{SamlProvider, SamlProviders};
use crate::{Error, Result};

/// The idempotent DDL of the table (kept by `spot-sso-pg`).
pub use spot_sso_pg::SAML_DDL;

/// The most bytes a stored configuration may take.
const MAX_CONFIG: usize = MAX_SAML_CONFIG;

/// Why a store call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamlStoreError {
    /// The store could not answer; nothing was changed.
    Unavailable,
    /// A create found the provider already there.
    Exists,
    /// An update or delete named a version that is no longer the stored one, or no such provider.
    Stale,
    /// The configuration does not validate; nothing was stored.
    Invalid,
}

/// A SAML provider as stored (everything the host needs, in a serde shape).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSaml {
    /// The display name for the sign-in page.
    pub name: String,
    /// The SP entity id.
    pub sp_entity_id: String,
    /// The assertion consumer service URL.
    pub sp_acs_url: String,
    /// The SP single-logout URL.
    #[serde(default)]
    pub sp_slo_url: Option<String>,
    /// The SP signing certificate (DER, hex).
    #[serde(default)]
    pub sp_signing_certificate: Option<String>,
    /// The SP encryption certificate (DER, hex).
    #[serde(default)]
    pub sp_encryption_certificate: Option<String>,
    /// Whether `AuthnRequest`s are signed.
    #[serde(default)]
    pub sign_authn_requests: bool,
    /// The name of the SP signing key in the secrets service.
    #[serde(default)]
    pub signing_key_ref: Option<String>,
    /// The SP signing algorithm.
    #[serde(default)]
    pub signing_algorithm: Option<Jwa>,
    /// The tolerated clock skew, seconds.
    pub clock_skew_seconds: i64,
    /// The longest a logout request may live, seconds.
    pub max_logout_lifetime_seconds: i64,
    /// Whether IdP-initiated responses are accepted.
    #[serde(default)]
    pub allow_idp_initiated: bool,
    /// `assertion` or `assertion_or_response`.
    pub signature_policy: String,
    /// The size cap on a POSTed response.
    pub max_response_bytes: usize,
    /// The IdP entity id.
    pub idp_entity_id: String,
    /// The IdP single-sign-on URL.
    pub idp_sso_url: String,
    /// `redirect` or `post`.
    pub idp_sso_binding: String,
    /// The IdP single-logout URL.
    #[serde(default)]
    pub idp_slo_url: Option<String>,
    /// The pinned IdP signing certificates (DER, hex).
    pub idp_signing_certificates: Vec<String>,
    /// The attribute mapping.
    pub claims_map: SamlClaimsMap,
    /// Required `AuthnContextClassRef`s.
    #[serde(default)]
    pub acr_values: Vec<String>,
}

impl StoredSaml {
    /// The stored form of `provider`.
    #[must_use]
    pub fn of(provider: &SamlProvider) -> Self {
        let (sp, idp) = (&provider.sp, &provider.idp);
        Self {
            name: provider.name.clone(),
            sp_entity_id: sp.entity_id.clone(),
            sp_acs_url: sp.acs_url.clone(),
            sp_slo_url: sp.slo_url.clone(),
            sp_signing_certificate: sp.signing_certificate.as_deref().map(hex),
            sp_encryption_certificate: sp.encryption_certificate.as_deref().map(hex),
            sign_authn_requests: sp.sign_authn_requests,
            signing_key_ref: sp.signing_key_ref.clone(),
            signing_algorithm: sp.signing_algorithm,
            clock_skew_seconds: sp.clock_skew_seconds,
            max_logout_lifetime_seconds: sp.max_logout_lifetime_seconds,
            allow_idp_initiated: sp.allow_idp_initiated,
            signature_policy: match sp.signature_policy {
                SignaturePolicy::Assertion => "assertion",
                SignaturePolicy::AssertionOrResponse => "assertion_or_response",
            }
            .to_owned(),
            max_response_bytes: sp.max_response_bytes,
            idp_entity_id: idp.entity_id.clone(),
            idp_sso_url: idp.sso_url.clone(),
            idp_sso_binding: match idp.sso_binding {
                SamlBinding::Redirect => "redirect",
                SamlBinding::Post => "post",
            }
            .to_owned(),
            idp_slo_url: idp.slo_url.clone(),
            idp_signing_certificates: idp.signing_certificates.iter().map(|c| hex(c)).collect(),
            claims_map: idp.claims_map.clone(),
            acr_values: idp.acr_values.clone(),
        }
    }

    /// The provider `id` of `audience` this row describes, or `None` when it does not parse.
    #[must_use]
    pub fn provider(&self, id: &ProviderId, audience: Audience) -> Option<SamlProvider> {
        let policy = match self.signature_policy.as_str() {
            "assertion" => SignaturePolicy::Assertion,
            "assertion_or_response" => SignaturePolicy::AssertionOrResponse,
            _ => return None,
        };
        let binding = match self.idp_sso_binding.as_str() {
            "redirect" => SamlBinding::Redirect,
            "post" => SamlBinding::Post,
            _ => return None,
        };
        let optional = |text: &Option<String>| -> Option<Option<Vec<u8>>> {
            match text {
                None => Some(None),
                Some(t) => unhex(t).map(Some),
            }
        };
        let certificates = self
            .idp_signing_certificates
            .iter()
            .map(|c| unhex(c))
            .collect::<Option<Vec<_>>>()?;
        let sp = SpConfig {
            entity_id: self.sp_entity_id.clone(),
            acs_url: self.sp_acs_url.clone(),
            slo_url: self.sp_slo_url.clone(),
            signing_certificate: optional(&self.sp_signing_certificate)?,
            encryption_certificate: optional(&self.sp_encryption_certificate)?,
            sign_authn_requests: self.sign_authn_requests,
            signing_key_ref: self.signing_key_ref.clone(),
            signing_algorithm: self.signing_algorithm,
            clock_skew_seconds: self.clock_skew_seconds,
            max_logout_lifetime_seconds: self.max_logout_lifetime_seconds,
            allow_idp_initiated: self.allow_idp_initiated,
            signature_policy: policy,
            max_response_bytes: self.max_response_bytes,
        };
        let mut idp = IdpConfig::new(
            id.clone(),
            self.idp_entity_id.clone(),
            self.idp_sso_url.clone(),
            certificates,
        );
        idp.sso_binding = binding;
        idp.slo_url = self.idp_slo_url.clone();
        idp.claims_map = self.claims_map.clone();
        idp.acr_values = self.acr_values.clone();
        sp.validate().ok()?;
        if idp.signing_certificates.is_empty() || idp.entity_id.is_empty() || idp.sso_url.is_empty()
        {
            return None;
        }
        Some(SamlProvider {
            sp,
            idp,
            name: self.name.clone(),
            audience,
        })
    }
}

/// A stored provider with its version.
#[derive(Clone, Debug)]
pub struct Versioned {
    /// The provider id.
    pub id: ProviderId,
    /// The stored version (the `ETag`).
    pub version: i64,
    /// The provider.
    pub provider: SamlProvider,
}

/// The SAML provider store of one tenant.
#[derive(Clone)]
pub struct PgSamlStore {
    rows: PgSamlRows,
}

impl std::fmt::Debug for PgSamlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSamlStore")
            .field("tenant", &self.rows.tenant())
            .finish_non_exhaustive()
    }
}

impl From<SamlRowError> for SamlStoreError {
    fn from(error: SamlRowError) -> Self {
        match error {
            SamlRowError::Unavailable => Self::Unavailable,
            SamlRowError::Exists => Self::Exists,
            SamlRowError::Stale => Self::Stale,
        }
    }
}

fn audience_of(text: &str) -> Option<Audience> {
    match text {
        "staff" => Some(Audience::Staff),
        "partner" => Some(Audience::Partner),
        _ => None,
    }
}

/// One stored row as a provider, or `None` when it cannot be trusted.
fn serve(id: &str, version: i64, audience: &str, config: &str) -> Option<Versioned> {
    let audience = audience_of(audience)?;
    let stored: StoredSaml = serde_json::from_str(config).ok()?;
    let id = ProviderId::new(id);
    let provider = stored.provider(&id, audience)?;
    Some(Versioned {
        id,
        version,
        provider,
    })
}

impl PgSamlStore {
    /// A store on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Option<Self> {
        PgSamlRows::new(pool, tenant).map(|rows| Self { rows })
    }

    /// Creates the table if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    ///
    /// `sso_providers_host.migration_failed`.
    pub async fn prepare(&self) -> Result<()> {
        self.rows.prepare().await.map_err(|_| {
            Error(
                "sso_providers_host.migration_failed: the sign-in tables could not be created"
                    .into(),
            )
        })
    }

    /// Every servable provider of the tenant, ordered by id.
    ///
    /// # Errors
    ///
    /// [`SamlStoreError::Unavailable`].
    pub async fn all(&self) -> std::result::Result<Vec<Versioned>, SamlStoreError> {
        Ok(self
            .rows
            .list()
            .await?
            .iter()
            .filter_map(|row| serve(&row.id, row.version, &row.audience, &row.config))
            .collect())
    }

    /// The provider `id`, `None` when the tenant has none or its row cannot be served.
    ///
    /// # Errors
    ///
    /// [`SamlStoreError::Unavailable`].
    pub async fn find(
        &self,
        id: &ProviderId,
    ) -> std::result::Result<Option<Versioned>, SamlStoreError> {
        Ok(self
            .rows
            .get(id.as_str())
            .await?
            .and_then(|row| serve(id.as_str(), row.version, &row.audience, &row.config)))
    }

    /// Stores `provider` under `id`. `expected = None` creates (an existing id is
    /// [`SamlStoreError::Exists`]); `Some(version)` replaces that version only
    /// ([`SamlStoreError::Stale`] otherwise). Returns the new version.
    ///
    /// # Errors
    ///
    /// [`SamlStoreError::Invalid`] (the provider does not round-trip through its stored form, so it
    /// would never be served), [`SamlStoreError::Exists`], [`SamlStoreError::Stale`],
    /// [`SamlStoreError::Unavailable`].
    pub async fn put(
        &self,
        id: &ProviderId,
        provider: &SamlProvider,
        expected: Option<i64>,
    ) -> std::result::Result<i64, SamlStoreError> {
        if provider.audience == Audience::Consumer {
            return Err(SamlStoreError::Invalid);
        }
        let stored = StoredSaml::of(provider);
        let text = serde_json::to_string(&stored).map_err(|_| SamlStoreError::Invalid)?;
        if text.len() > MAX_CONFIG || stored.provider(id, provider.audience).is_none() {
            return Err(SamlStoreError::Invalid);
        }
        let audience = provider.audience.as_str();
        Ok(match expected {
            None => self.rows.insert(id.as_str(), audience, &text).await?,
            Some(version) => {
                self.rows
                    .update(id.as_str(), audience, &text, version)
                    .await?
            }
        })
    }

    /// Deletes the provider if its version is `expected`.
    ///
    /// # Errors
    ///
    /// [`SamlStoreError::Stale`] (a different version, or none), [`SamlStoreError::Unavailable`].
    pub async fn delete(
        &self,
        id: &ProviderId,
        expected: i64,
    ) -> std::result::Result<(), SamlStoreError> {
        Ok(self.rows.delete(id.as_str(), expected).await?)
    }
}

impl SamlProviders for PgSamlStore {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a ProviderId,
    ) -> BoxFuture<'a, std::result::Result<Option<SamlProvider>, SsoFailure>> {
        Box::pin(async move {
            // A store serves its own tenant; another site's request finds nothing.
            if ctx.site.as_str() != self.rows.tenant() {
                return Ok(None);
            }
            self.find(id)
                .await
                .map(|found| found.map(|v| v.provider))
                .map_err(|_| SsoFailure::Unavailable)
        })
    }

    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<Vec<(ProviderId, String)>, SsoFailure>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.rows.tenant() {
                return Ok(Vec::new());
            }
            self.all()
                .await
                .map(|all| all.into_iter().map(|v| (v.id, v.provider.name)).collect())
                .map_err(|_| SsoFailure::Unavailable)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    pub(crate) fn sample(id: &str) -> SamlProvider {
        let sp = SpConfig {
            entity_id: "https://erp.acme.example/saml/sp".to_owned(),
            acs_url: format!("https://erp.acme.example/api/v1/auth/sso/{id}/acs"),
            ..SpConfig::default()
        };
        let idp = IdpConfig::new(
            ProviderId::new(id),
            "https://idp.example/entity",
            "https://idp.example/sso",
            vec![vec![0x30, 0x82, 0x01, 0x0a]],
        );
        SamlProvider {
            sp,
            idp,
            name: "Acme IdP".to_owned(),
            audience: Audience::Staff,
        }
    }

    #[test]
    fn a_provider_round_trips_through_its_stored_form() {
        let provider = sample("okta");
        let stored = StoredSaml::of(&provider);
        let text = serde_json::to_string(&stored).unwrap();
        let back: StoredSaml = serde_json::from_str(&text).unwrap();
        let again = back
            .provider(&ProviderId::new("okta"), Audience::Staff)
            .unwrap();
        assert_eq!(again.sp, provider.sp);
        assert_eq!(again.idp, provider.idp);
        assert_eq!(again.name, provider.name);
    }

    #[test]
    fn a_row_that_cannot_be_trusted_is_not_served() {
        let good = serde_json::to_string(&StoredSaml::of(&sample("okta"))).unwrap();
        assert!(serve("okta", 1, "staff", &good).is_some());
        // The audience column disagrees with nothing here, but an unknown one is refused.
        assert!(serve("okta", 1, "consumer", &good).is_none());
        assert!(serve("okta", 1, "staff", "{").is_none());
        // No pinned certificate: not served.
        let mut stored = StoredSaml::of(&sample("okta"));
        stored.idp_signing_certificates.clear();
        assert!(serve("okta", 1, "staff", &serde_json::to_string(&stored).unwrap()).is_none());
        // A certificate that is not hex.
        let mut stored = StoredSaml::of(&sample("okta"));
        stored.idp_signing_certificates = vec!["zz".to_owned()];
        assert!(serve("okta", 1, "staff", &serde_json::to_string(&stored).unwrap()).is_none());
        // An unknown policy word.
        let mut stored = StoredSaml::of(&sample("okta"));
        stored.signature_policy = "none".to_owned();
        assert!(serve("okta", 1, "staff", &serde_json::to_string(&stored).unwrap()).is_none());
    }
}
