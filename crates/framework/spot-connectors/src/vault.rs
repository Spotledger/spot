//! The credential vault: references the connector declares, resolved through `spot-secrets` at
//! call time.
//!
//! A connector never names a credential value, only a [`CredentialRef`]. The host resolves it
//! when it makes the outbound request. For `bearer`, `basic` and `oauth2` the host injects the
//! credential into the request itself, so the guest never holds the token; `oauth2` tokens are
//! cached in memory only and refreshed by the host, once under concurrency.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use spot_secrets::{KeyProvider, Sealed, SecretBox, SecretString};
use tokio::sync::Mutex;

use crate::error::{ConnectorError, Result};
use crate::model::AuthKind;

/// A reference to a credential by name. It is never the value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Wraps `name`. An empty name is refused.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        (!name.is_empty()).then_some(Self(name))
    }

    /// The reference name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolves credential references to values. Async because a value may be unsealed
/// (`spot-secrets`) or an OAuth2 token refreshed over the network.
#[async_trait]
pub trait CredentialVault: Send + Sync {
    /// Resolves `reference` at `now_micros`. The value is zeroed on drop and never logged.
    async fn resolve(&self, reference: &CredentialRef, now_micros: i64) -> Result<SecretString>;

    /// Resolves `reference` for `tenant`. A vault that holds per-tenant state (a cached OAuth2
    /// token) must key it by `tenant`; the default suits a vault with none.
    async fn resolve_for(
        &self,
        _tenant: &str,
        reference: &CredentialRef,
        now_micros: i64,
    ) -> Result<SecretString> {
        self.resolve(reference, now_micros).await
    }
}

/// How one credential is stored.
enum Entry {
    /// A static value, already unsealed.
    Static(SecretString),
    /// An OAuth2 client-credentials flow, refreshed by the vault.
    OAuth2(OAuth2Config),
}

/// The OAuth2 client-credentials configuration for one reference.
#[derive(Clone, Debug)]
pub struct OAuth2Config {
    /// The token endpoint.
    pub token_url: String,
    /// The client id, a static value (not a secret).
    pub client_id: String,
    /// The client secret, by reference.
    pub client_secret: CredentialRef,
    /// The scopes to request.
    pub scopes: Vec<String>,
}

/// A cached access token. In memory only; never persisted.
struct CachedToken {
    value: SecretString,
    /// The instant it expires, in microseconds since the Unix epoch.
    expires_at_micros: i64,
}

/// The transport the vault uses for a token refresh. The same [`crate::HttpService`] the host
/// uses for connector requests, so a refresh is subject to the egress policy too.
#[async_trait]
pub trait TokenTransport: Send + Sync {
    /// POSTs `form` (already `application/x-www-form-urlencoded`) to `url`, returning the JSON
    /// body.
    async fn post_form(&self, url: &str, form: &str, now_micros: i64) -> Result<String>;
}

/// The vault: named entries, an OAuth2 token cache and the transport a refresh uses.
pub struct Vault {
    entries: HashMap<String, Entry>,
    transport: Option<Arc<dyn TokenTransport>>,
    /// Keyed by `(tenant, reference)`: a token minted for one tenant is never served to another.
    cache: Mutex<HashMap<(String, String), CachedToken>>,
    refreshes: AtomicU64,
    /// Refresh this many microseconds before the stated expiry, so a token in flight does not
    /// expire mid-request.
    skew_micros: i64,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("entries", &self.entries.len())
            .field("has_transport", &self.transport.is_some())
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// An empty vault with no refresh transport.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            transport: None,
            cache: Mutex::new(HashMap::new()),
            refreshes: AtomicU64::new(0),
            skew_micros: 60_000_000,
        }
    }

    /// Sets the transport a token refresh uses.
    #[must_use]
    pub fn with_transport(mut self, transport: Arc<dyn TokenTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Adds a static credential under `reference`.
    pub fn insert_static(&mut self, reference: CredentialRef, value: SecretString) {
        self.entries.insert(reference.0, Entry::Static(value));
    }

    /// Adds an OAuth2 credential under `reference`.
    pub fn insert_oauth2(&mut self, reference: CredentialRef, config: OAuth2Config) {
        self.entries.insert(reference.0, Entry::OAuth2(config));
    }

    /// How many times a token has actually been refreshed. Exposed for tests and monitoring.
    pub fn refresh_count(&self) -> u64 {
        self.refreshes.load(Ordering::SeqCst)
    }

    /// Whether `reference` is known to the vault.
    pub fn contains(&self, reference: &CredentialRef) -> bool {
        self.entries.contains_key(reference.as_str())
    }

    async fn refresh(
        &self,
        _reference: &CredentialRef,
        config: &OAuth2Config,
        now_micros: i64,
    ) -> Result<CachedToken> {
        let transport = self
            .transport
            .as_ref()
            .ok_or_else(|| ConnectorError::OAuthRefresh {
                reason: "no token transport is configured".into(),
            })?;
        let secret = match self.entries.get(config.client_secret.as_str()) {
            Some(Entry::Static(s)) => clone_secret(s),
            _ => {
                return Err(ConnectorError::SecretUnavailable {
                    reference: config.client_secret.to_string(),
                });
            }
        };
        let form = url_encode(&[
            ("grant_type", "client_credentials"),
            ("client_id", &config.client_id),
            ("client_secret", secret.expose()),
            ("scope", &config.scopes.join(" ")),
        ]);
        let body = transport
            .post_form(&config.token_url, &form, now_micros)
            .await?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|_| ConnectorError::OAuthRefresh {
                reason: "the token response is not JSON".into(),
            })?;
        let access = parsed
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ConnectorError::OAuthRefresh {
                reason: "the token response has no access_token".into(),
            })?;
        let expires_in = parsed
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(3600);
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let lifetime = expires_in.saturating_mul(1_000_000);
        Ok(CachedToken {
            value: SecretString::new(access),
            expires_at_micros: now_micros
                .saturating_add(lifetime)
                .saturating_sub(self.skew_micros.min(lifetime / 2)),
        })
    }
}

impl Default for Vault {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CredentialVault for Vault {
    /// Resolves without a tenant: an OAuth2 token is cached in the tenant-less bucket, which no
    /// tenant's call ever reads.
    async fn resolve(&self, reference: &CredentialRef, now_micros: i64) -> Result<SecretString> {
        self.resolve_for("", reference, now_micros).await
    }

    async fn resolve_for(
        &self,
        tenant: &str,
        reference: &CredentialRef,
        now_micros: i64,
    ) -> Result<SecretString> {
        match self.entries.get(reference.as_str()) {
            None => Err(ConnectorError::SecretUnavailable {
                reference: reference.to_string(),
            }),
            Some(Entry::Static(value)) => Ok(clone_secret(value)),
            Some(Entry::OAuth2(config)) => {
                // The lock is held across the refresh, so concurrent callers wait for the one
                // refresh instead of each starting their own.
                let mut cache = self.cache.lock().await;
                let key = (tenant.to_owned(), reference.as_str().to_owned());
                if let Some(token) = cache.get(&key)
                    && now_micros < token.expires_at_micros
                {
                    return Ok(clone_secret(&token.value));
                }
                let fresh = self.refresh(reference, config, now_micros).await?;
                let value = clone_secret(&fresh.value);
                cache.insert(key, fresh);
                Ok(value)
            }
        }
    }
}

/// An [`AuthSpec`] says how a connector authenticates one credential.
#[derive(Clone, Debug)]
pub struct AuthSpec {
    /// The scheme.
    pub kind: AuthKind,
    /// The credential reference.
    pub credential: CredentialRef,
    /// For `basic`, the static username.
    pub username: Option<String>,
    /// The header to write, default `Authorization`.
    pub header: Option<String>,
}

impl AuthSpec {
    /// The header name and value for this scheme, resolving the credential through `vault`.
    /// The value is the only place the secret appears, and it never leaves the host.
    pub async fn header(
        &self,
        vault: &dyn CredentialVault,
        tenant: &str,
        now_micros: i64,
    ) -> Result<(String, String)> {
        let header = self
            .header
            .clone()
            .unwrap_or_else(|| "Authorization".to_owned());
        let secret = vault
            .resolve_for(tenant, &self.credential, now_micros)
            .await?;
        let value = match self.kind {
            AuthKind::None => return Ok((header, String::new())),
            AuthKind::Bearer | AuthKind::OAuth2 => format!("Bearer {}", secret.expose()),
            AuthKind::Basic => {
                let user = self.username.clone().unwrap_or_default();
                let joined = format!("{user}:{}", secret.expose());
                format!("Basic {}", STANDARD.encode(joined))
            }
        };
        Ok((header, value))
    }
}

/// A vault whose values are sealed with `spot-secrets` and opened **at call time**. This is the
/// form a real deployment uses: only the [`Sealed`] value is stored, and a copy moved to another
/// reference fails to open (the associated data binds it).
pub struct SealedVault<P: KeyProvider> {
    boxed: SecretBox<P>,
    entries: HashMap<String, (Sealed, Vec<u8>)>,
}

impl<P: KeyProvider> SealedVault<P> {
    /// A vault that seals and opens with `provider`'s key-encryption key.
    pub fn new(provider: P) -> Self {
        Self {
            boxed: SecretBox::new(provider),
            entries: HashMap::new(),
        }
    }

    /// Seals `value` under `reference` and stores it, bound to `aad`.
    pub async fn insert(
        &mut self,
        reference: CredentialRef,
        value: &SecretString,
        aad: &[u8],
    ) -> Result<()> {
        let sealed =
            self.boxed
                .seal(value, aad)
                .await
                .map_err(|_| ConnectorError::HostFailure {
                    reason: "the credential could not be sealed".into(),
                })?;
        self.entries.insert(reference.0, (sealed, aad.to_vec()));
        Ok(())
    }

    /// Stores an already-sealed value.
    pub fn insert_sealed(&mut self, reference: CredentialRef, sealed: Sealed, aad: Vec<u8>) {
        self.entries.insert(reference.0, (sealed, aad));
    }
}

#[async_trait]
impl<P: KeyProvider + 'static> CredentialVault for SealedVault<P> {
    async fn resolve(&self, reference: &CredentialRef, _now_micros: i64) -> Result<SecretString> {
        match self.entries.get(reference.as_str()) {
            None => Err(ConnectorError::SecretUnavailable {
                reference: reference.to_string(),
            }),
            Some((sealed, aad)) => {
                self.boxed
                    .open(sealed, aad)
                    .await
                    .map_err(|_| ConnectorError::SecretUnavailable {
                        reference: reference.to_string(),
                    })
            }
        }
    }
}

/// A copy of a secret value (the stored value is not `Clone`, by design).
fn clone_secret(value: &SecretString) -> SecretString {
    SecretString::new(value.expose())
}
/// Percent-encodes a form body. Only what a client-credentials form needs is encoded.
fn url_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&encode_component(key));
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out
}

fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
