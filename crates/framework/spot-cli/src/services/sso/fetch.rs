//! Outbound fetches of the SSO host: discovery, JWKS, the token endpoint and SAML metadata.
//!
//! Every call goes through the egress guard ([`spot_egress::Guard`]): the host name is resolved
//! once, every answer is checked against the policy (no private, loopback or link-local address
//! unless the site named that provider host and its addresses), and the connection is made to
//! the checked address only. On top of that this module enforces what the guard does not:
//!
//! - **HTTPS only** (a plain `http` URL is refused before any lookup);
//! - a **timeout** for the whole exchange and a **body cap**;
//! - **no redirects**: the sender returns a `3xx` as a response and every non-`2xx` is a refusal;
//! - a **bounded cache** ([`ProviderCache`]) of each provider's discovery document and keys, with
//!   a refetch limit, so an attacker's unknown `kid` cannot make the server fetch in a loop.
//!
//! Nothing here logs a URL, a body, a form field or a header: an error is a fixed class.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::HeaderMap;
use spot_egress::{Bytes, EgressError, Guard, HttpRequest, Method, Purpose, Sender};
use spot_http::api::service::BoxFuture;
use spot_sso::{Discovery, KeyCache};
use url::Url;

/// Why a fetch did not produce a body. Carries no URL, address or detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The egress policy refused the destination (address, port, policy, rebinding).
    Refused,
    /// Not HTTPS, or not a URL.
    NotHttps,
    /// The exchange took longer than the limit.
    Timeout,
    /// The body was larger than the cap.
    TooLarge,
    /// The server answered something other than `2xx` (a redirect is one).
    BadStatus,
    /// The connection failed.
    Unavailable,
}

/// The bounds of one exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLimits {
    /// The whole exchange, from connect to the last body byte.
    pub timeout: Duration,
    /// The most response bytes read.
    pub max_bytes: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_bytes: 256 * 1024,
        }
    }
}

/// The outbound calls of the host, as a port so a test scripts the identity provider.
pub trait Fetcher: Send + Sync + 'static {
    /// `GET url` for `tenant`; the body on `2xx`.
    fn get<'a>(
        &'a self,
        tenant: &'a str,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>>;

    /// `POST url` with `form` as `application/x-www-form-urlencoded`; the body on `2xx`.
    /// `form` may hold a client secret and a code: it is never logged.
    fn post_form<'a>(
        &'a self,
        tenant: &'a str,
        url: &'a str,
        form: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>>;
}

/// The production [`Fetcher`]: the egress guard and a sender that connects to checked addresses.
pub struct EgressFetcher {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    limits: FetchLimits,
}

impl EgressFetcher {
    /// A fetcher over `guard` and `sender`, bounded by `limits`.
    #[must_use]
    pub fn new(guard: Arc<Guard>, sender: Arc<dyn Sender>, limits: FetchLimits) -> Self {
        Self {
            guard,
            sender,
            limits,
        }
    }

    async fn exchange(&self, tenant: &str, request: HttpRequest) -> Result<Vec<u8>, FetchError> {
        if request.url.scheme() != "https" {
            return Err(FetchError::NotHttps);
        }
        let authorized = self
            .guard
            .authorize(&spot_egress::Request {
                url: request.url.clone(),
                purpose: Purpose::Connector,
                tenant: tenant.to_owned(),
                actor: None,
            })
            .await
            .map_err(|_| FetchError::Refused)?;
        let sent = tokio::time::timeout(
            self.limits.timeout,
            self.sender
                .send_request(&request, &authorized, self.limits.max_bytes),
        )
        .await
        .map_err(|_| FetchError::Timeout)?;
        let response = sent.map_err(|error| match error {
            EgressError::ResponseTooLarge => FetchError::TooLarge,
            _ => FetchError::Unavailable,
        })?;
        if !response.status.is_success() {
            return Err(FetchError::BadStatus);
        }
        Ok(response.body)
    }
}

impl Fetcher for EgressFetcher {
    fn get<'a>(
        &'a self,
        tenant: &'a str,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            let url = Url::parse(url).map_err(|_| FetchError::NotHttps)?;
            self.exchange(tenant, HttpRequest::get(url, HeaderMap::new()))
                .await
        })
    }

    fn post_form<'a>(
        &'a self,
        tenant: &'a str,
        url: &'a str,
        form: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            let url = Url::parse(url).map_err(|_| FetchError::NotHttps)?;
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(form.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
            headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/json"),
            );
            self.exchange(
                tenant,
                HttpRequest::new(Method::Post, url, headers, Some(Bytes::from(body))),
            )
            .await
        })
    }
}

/// One provider's discovery document and keys.
struct Entry {
    discovery: Discovery,
    keys: KeyCache,
    fetched_at: i64,
    last_refetch: i64,
}

/// The bounds of [`ProviderCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheLimits {
    /// The most providers cached (the oldest entry goes first).
    pub max_entries: usize,
    /// The most keys one provider may publish; a larger set is refused.
    pub max_keys: usize,
    /// How long an entry is used before it is fetched again, in seconds.
    pub max_age_seconds: i64,
    /// The shortest time between two forced refetches of one provider, in seconds.
    pub refetch_min_seconds: i64,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_entries: 64,
            max_keys: 32,
            max_age_seconds: 3600,
            refetch_min_seconds: 60,
        }
    }
}

/// A bounded cache of discovery documents and keys, per provider.
pub struct ProviderCache {
    limits: CacheLimits,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl ProviderCache {
    /// An empty cache.
    #[must_use]
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            limits,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// The bounds.
    #[must_use]
    pub fn limits(&self) -> CacheLimits {
        self.limits
    }

    /// The cached discovery and keys of `provider`, when younger than the maximum age.
    #[must_use]
    pub fn get(&self, provider: &str, now: i64) -> Option<(Discovery, KeyCache)> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .get(provider)
            .filter(|e| now >= e.fetched_at && now - e.fetched_at < self.limits.max_age_seconds)
            .map(|e| (e.discovery.clone(), e.keys.clone()))
    }

    /// Stores a fresh fetch, evicting the oldest entry when the cache is full. `false` (and
    /// nothing stored) when the key set is larger than the bound.
    pub fn put(&self, provider: &str, discovery: Discovery, keys: KeyCache, now: i64) -> bool {
        if keys.current_len() > self.limits.max_keys {
            return false;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if !entries.contains_key(provider) && entries.len() >= self.limits.max_entries {
            let oldest = entries
                .iter()
                .min_by_key(|(_, e)| e.fetched_at)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        let last_refetch = entries.get(provider).map_or(i64::MIN, |e| e.last_refetch);
        entries.insert(
            provider.to_owned(),
            Entry {
                discovery,
                keys,
                fetched_at: now,
                last_refetch,
            },
        );
        true
    }

    /// Whether a forced refetch of `provider` is allowed now; if so it is counted, so the next
    /// one waits the minimum interval. A token naming an unknown `kid` therefore costs one fetch
    /// per interval per provider, however many such tokens arrive.
    pub fn allow_refetch(&self, provider: &str, now: i64) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        match entries.get_mut(provider) {
            Some(entry)
                if now.saturating_sub(entry.last_refetch) >= self.limits.refetch_min_seconds =>
            {
                entry.last_refetch = now;
                true
            }
            Some(_) => false,
            // Nothing cached: the first fetch is not a refetch.
            None => true,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_core::Audience;
    use spot_sso::Jwks;
    use spot_sso::contract_stores::sample_provider;

    fn discovery() -> Discovery {
        let config = sample_provider("acme", Audience::Staff);
        let body = serde_json::json!({
            "issuer": "https://idp.example.com",
            "authorization_endpoint": "https://idp.example.com/auth",
            "token_endpoint": "https://idp.example.com/token",
            "jwks_uri": "https://idp.example.com/jwks",
            "code_challenge_methods_supported": ["S256"],
            "response_types_supported": ["code"]
        });
        Discovery::parse(body.to_string().as_bytes(), &config).unwrap()
    }

    fn keys() -> KeyCache {
        KeyCache::new(Jwks::default(), 600)
    }

    #[test]
    fn an_entry_ages_out() {
        let cache = ProviderCache::new(CacheLimits::default());
        assert!(cache.put("acme", discovery(), keys(), 1_000));
        assert!(cache.get("acme", 1_000 + 3_599).is_some());
        assert!(cache.get("acme", 1_000 + 3_600).is_none());
        assert!(cache.get("other", 1_000).is_none());
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = ProviderCache::new(CacheLimits {
            max_entries: 2,
            ..CacheLimits::default()
        });
        assert!(cache.put("a", discovery(), keys(), 1));
        assert!(cache.put("b", discovery(), keys(), 2));
        assert!(cache.put("c", discovery(), keys(), 3));
        assert!(cache.get("a", 4).is_none());
        assert!(cache.get("b", 4).is_some() && cache.get("c", 4).is_some());
    }

    #[test]
    fn a_forced_refetch_is_rate_limited() {
        let cache = ProviderCache::new(CacheLimits::default());
        assert!(cache.allow_refetch("acme", 10), "nothing cached yet");
        cache.put("acme", discovery(), keys(), 10);
        assert!(cache.allow_refetch("acme", 10));
        assert!(
            !cache.allow_refetch("acme", 50),
            "within the minimum interval"
        );
        assert!(cache.allow_refetch("acme", 70));
    }
}
