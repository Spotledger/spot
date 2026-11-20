//! The Vault Transit client: encrypt, decrypt and key read over an egress `Sender`.

use crate::client::{KeyInfo, KmsCiphertext, KmsClient};
use crate::error::KmsError;
use crate::fsutil::read_private;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use spot_egress::{
    Authorized, Bytes, EgressError, Guard, HttpRequest, Method, Purpose, Request, Sender,
};
use spot_secrets::SecretBytes;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use url::Url;
use zeroize::Zeroizing;

/// The most a KMS answer may carry, in bytes.
const MAX_RESPONSE: u64 = 64 * 1024;
/// The longest token file, in bytes.
const MAX_TOKEN_FILE: u64 = 4096;
/// The longest ciphertext string this client accepts, in bytes.
const MAX_CIPHERTEXT: usize = 16 * 1024;
/// The longest plaintext this client sends, in bytes.
const MAX_PLAINTEXT: usize = 4096;

/// Decides whether a request to a URL may go out, and to which pinned addresses.
#[async_trait::async_trait]
pub trait EgressGate: Send + Sync {
    /// Authorises `url`, or refuses it.
    async fn authorize(&self, url: &Url) -> Result<Authorized, EgressError>;
}

/// The production gate: the platform's egress [`Guard`], for one tenant, with the
/// connector purpose. The tenant needs an allow entry for the KMS host (see the README).
pub struct GuardGate {
    guard: Arc<Guard>,
    tenant: String,
}

impl GuardGate {
    /// A gate over `guard` for `tenant`.
    pub fn new(guard: Arc<Guard>, tenant: impl Into<String>) -> Self {
        Self {
            guard,
            tenant: tenant.into(),
        }
    }
}

#[async_trait::async_trait]
impl EgressGate for GuardGate {
    async fn authorize(&self, url: &Url) -> Result<Authorized, EgressError> {
        self.guard
            .authorize(&Request {
                url: url.clone(),
                purpose: Purpose::Connector,
                tenant: self.tenant.clone(),
                actor: None,
            })
            .await
    }
}

/// Configuration of a [`TransitClient`].
#[derive(Debug, Clone)]
pub struct TransitConfig {
    /// The KMS base address, `https` only, no user info, query or fragment.
    pub address: String,
    /// The transit secrets-engine mount, for example `transit`.
    pub mount: String,
    /// The key's name in the mount.
    pub key_name: String,
    /// The file holding the token. It must be a regular file with no group or other access; the
    /// token is never taken from an environment variable or an argument.
    pub token_file: PathBuf,
    /// An optional Vault namespace, sent as `X-Vault-Namespace`.
    pub namespace: Option<String>,
    /// The longest one KMS call may take, connection to last byte.
    pub request_timeout: Duration,
}

struct Checked {
    base: Url,
    mount: Vec<String>,
    key_name: String,
    namespace: Option<HeaderValue>,
}

fn segment_ok(text: &str, allow_dot: bool) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text != ".."
        && text != "."
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (allow_dot && b == b'.')
        })
}

fn check(config: &TransitConfig) -> Result<Checked, KmsError> {
    let base = Url::parse(&config.address)
        .map_err(|_| KmsError::ConfigInvalid("the KMS address is not a URL"))?;
    if base.scheme() != "https" {
        return Err(KmsError::ConfigInvalid("the KMS address must use https"));
    }
    if base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(KmsError::ConfigInvalid(
            "the KMS address must be a bare host with no user info, query or fragment",
        ));
    }
    let mount: Vec<String> = config.mount.split('/').map(str::to_owned).collect();
    if mount.is_empty() || mount.len() > 4 || !mount.iter().all(|s| segment_ok(s, true)) {
        return Err(KmsError::ConfigInvalid(
            "the transit mount is not acceptable",
        ));
    }
    if !segment_ok(&config.key_name, false) {
        return Err(KmsError::ConfigInvalid("the key name is not acceptable"));
    }
    if config.request_timeout.is_zero() || config.request_timeout > Duration::from_secs(300) {
        return Err(KmsError::ConfigInvalid(
            "the request timeout must be between 1 ms and 5 minutes",
        ));
    }
    let namespace = match &config.namespace {
        None => None,
        Some(ns) => {
            let ok = !ns.is_empty()
                && ns.len() <= 128
                && ns
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/'));
            if !ok {
                return Err(KmsError::ConfigInvalid("the namespace is not acceptable"));
            }
            Some(
                HeaderValue::from_str(ns)
                    .map_err(|_| KmsError::ConfigInvalid("the namespace is not acceptable"))?,
            )
        }
    };
    Ok(Checked {
        base,
        mount,
        key_name: config.key_name.clone(),
        namespace,
    })
}

async fn read_token(path: &std::path::Path) -> Result<Zeroizing<String>, KmsError> {
    let raw = read_private(path, MAX_TOKEN_FILE)
        .await
        .map_err(KmsError::TokenUnreadable)?;
    let text = std::str::from_utf8(raw.as_slice())
        .map_err(|_| KmsError::TokenUnreadable("the token is not text"))?
        .trim();
    if text.is_empty() {
        return Err(KmsError::TokenUnreadable("the token file is empty"));
    }
    if !text.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(KmsError::TokenUnreadable(
            "the token holds characters a token cannot hold",
        ));
    }
    Ok(Zeroizing::new(text.to_owned()))
}

/// A [`KmsClient`] for the Vault Transit API.
///
/// It never creates an HTTP client of its own: every call is authorised by the [`EgressGate`]
/// and sent by the caller's [`Sender`]. The token is held in a zeroising string and is in no
/// `Debug`, error or log.
pub struct TransitClient {
    checked: Checked,
    token_file: PathBuf,
    token: RwLock<Zeroizing<String>>,
    timeout: Duration,
    sender: Arc<dyn Sender>,
    gate: Arc<dyn EgressGate>,
}

impl std::fmt::Debug for TransitClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransitClient")
            .field("key_name", &self.checked.key_name)
            .finish_non_exhaustive()
    }
}

impl TransitClient {
    /// Validates `config` and reads the token file. It makes no network call.
    ///
    /// # Errors
    /// `secrets_kms.config_invalid` for a bad address, mount, key name, namespace or timeout;
    /// `secrets_kms.token_unreadable` when the token file is missing, not a regular file,
    /// readable by group or other, empty or too large.
    pub async fn open(
        config: TransitConfig,
        sender: Arc<dyn Sender>,
        gate: Arc<dyn EgressGate>,
    ) -> Result<Self, KmsError> {
        let checked = check(&config)?;
        let token = read_token(&config.token_file).await?;
        Ok(Self {
            checked,
            token_file: config.token_file,
            token: RwLock::new(token),
            timeout: config.request_timeout,
            sender,
            gate,
        })
    }

    /// Re-reads the token file, for a rotated token. The old token stays in use when the read
    /// fails.
    pub async fn reload_token(&self) -> Result<(), KmsError> {
        let fresh = read_token(&self.token_file).await?;
        *self
            .token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
        Ok(())
    }

    fn url(&self, verb: &str, with_key: bool) -> Result<Url, KmsError> {
        let mut url = self.checked.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| KmsError::ConfigInvalid("the KMS address cannot take a path"))?;
            segments.pop_if_empty();
            segments.push("v1");
            for part in &self.checked.mount {
                segments.push(part);
            }
            segments.push(verb);
            if with_key {
                segments.push(&self.checked.key_name);
            }
        }
        Ok(url)
    }

    fn headers(&self) -> Result<HeaderMap, KmsError> {
        let mut headers = HeaderMap::new();
        let token = self
            .token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut value = HeaderValue::from_str(token.as_str())
            .map_err(|_| KmsError::TokenUnreadable("the token cannot be sent as a header"))?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-vault-token"), value);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ns) = &self.checked.namespace {
            headers.insert(HeaderName::from_static("x-vault-namespace"), ns.clone());
        }
        Ok(headers)
    }

    /// One call; returns the `data` object of a success answer.
    async fn call(
        &self,
        method: Method,
        verb: &str,
        with_key: bool,
        body: Option<Value>,
    ) -> Result<Map<String, Value>, KmsError> {
        let url = self.url(verb, with_key)?;
        let authorized = self.gate.authorize(&url).await.map_err(map_egress)?;
        let body = match body {
            Some(value) => {
                Some(Bytes::from(serde_json::to_vec(&value).map_err(|_| {
                    KmsError::InputInvalid("the request cannot be encoded")
                })?))
            }
            None => None,
        };
        let request = HttpRequest::new(method, url, self.headers()?, body);
        let response = tokio::time::timeout(
            self.timeout,
            self.sender
                .send_request(&request, &authorized, MAX_RESPONSE),
        )
        .await
        .map_err(|_| KmsError::Unreachable("the KMS did not answer in time"))?
        .map_err(map_egress)?;
        if !response.status.is_success() {
            // The body is dropped unread: an error body can echo what was sent.
            return Err(KmsError::Rejected {
                status: response.status.as_u16(),
            });
        }
        if response.body.len() as u64 > MAX_RESPONSE {
            return Err(KmsError::ResponseInvalid("the answer is too large"));
        }
        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|_| KmsError::ResponseInvalid("the answer is not JSON"))?;
        match parsed {
            Value::Object(mut top) => match top.remove("data") {
                Some(Value::Object(data)) => Ok(data),
                _ => Err(KmsError::ResponseInvalid("the answer has no data object")),
            },
            _ => Err(KmsError::ResponseInvalid("the answer is not an object")),
        }
    }
}

fn map_egress(error: EgressError) -> KmsError {
    match error {
        EgressError::AddressDenied { .. }
        | EgressError::PolicyDenied
        | EgressError::PortDenied { .. }
        | EgressError::ResolutionMixed
        | EgressError::RedirectDenied
        | EgressError::InvalidUrl => KmsError::EgressRefused,
        EgressError::ResponseTooLarge => KmsError::ResponseInvalid("the answer is too large"),
        _ => KmsError::Unreachable("the KMS could not be reached"),
    }
}

/// The version in `vault:v<N>:...`.
fn version_of(ciphertext: &str) -> Option<u32> {
    let rest = ciphertext.strip_prefix("vault:v")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with(':') {
        return None;
    }
    digits.parse().ok().filter(|v| *v >= 1)
}

fn u32_field(data: &Map<String, Value>, name: &str) -> Option<u32> {
    data.get(name)?.as_u64().and_then(|n| u32::try_from(n).ok())
}

#[async_trait::async_trait]
impl KmsClient for TransitClient {
    fn key_name(&self) -> &str {
        &self.checked.key_name
    }

    async fn encrypt(&self, aad: &[u8], plaintext: &[u8]) -> Result<KmsCiphertext, KmsError> {
        if plaintext.is_empty() || plaintext.len() > MAX_PLAINTEXT {
            return Err(KmsError::InputInvalid("the value length is out of range"));
        }
        let mut body = Map::new();
        body.insert(
            "plaintext".into(),
            Value::String(STANDARD.encode(plaintext)),
        );
        if !aad.is_empty() {
            body.insert("context".into(), Value::String(STANDARD.encode(aad)));
        }
        let data = self
            .call(Method::Post, "encrypt", true, Some(Value::Object(body)))
            .await?;
        let text = data
            .get("ciphertext")
            .and_then(Value::as_str)
            .ok_or(KmsError::ResponseInvalid("the answer has no ciphertext"))?;
        if text.len() > MAX_CIPHERTEXT || !text.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(KmsError::ResponseInvalid(
                "the ciphertext is not acceptable",
            ));
        }
        let from_prefix =
            version_of(text).ok_or(KmsError::ResponseInvalid("the ciphertext has no version"))?;
        let key_version = u32_field(&data, "key_version").unwrap_or(from_prefix);
        if key_version != from_prefix {
            return Err(KmsError::ResponseInvalid(
                "the ciphertext and the answer disagree on the version",
            ));
        }
        Ok(KmsCiphertext {
            key_version,
            bytes: text.as_bytes().to_vec(),
        })
    }

    async fn decrypt(
        &self,
        aad: &[u8],
        ciphertext: &KmsCiphertext,
    ) -> Result<SecretBytes, KmsError> {
        let text = std::str::from_utf8(&ciphertext.bytes)
            .map_err(|_| KmsError::InputInvalid("the ciphertext is not a transit ciphertext"))?;
        if text.len() > MAX_CIPHERTEXT
            || !text.bytes().all(|b| (0x21..=0x7e).contains(&b))
            || version_of(text) != Some(ciphertext.key_version)
        {
            return Err(KmsError::InputInvalid(
                "the ciphertext is not a transit ciphertext",
            ));
        }
        let mut body = Map::new();
        body.insert("ciphertext".into(), json!(text));
        if !aad.is_empty() {
            body.insert("context".into(), Value::String(STANDARD.encode(aad)));
        }
        let data = self
            .call(Method::Post, "decrypt", true, Some(Value::Object(body)))
            .await?;
        let encoded = data
            .get("plaintext")
            .and_then(Value::as_str)
            .map(|s| Zeroizing::new(s.to_owned()))
            .ok_or(KmsError::ResponseInvalid("the answer has no plaintext"))?;
        let plain = STANDARD
            .decode(encoded.as_bytes())
            .map(Zeroizing::new)
            .map_err(|_| KmsError::ResponseInvalid("the plaintext is not base64"))?;
        Ok(SecretBytes::new(plain.to_vec()))
    }

    async fn key_info(&self) -> Result<KeyInfo, KmsError> {
        let data = self.call(Method::Get, "keys", true, None).await?;
        let latest_version = u32_field(&data, "latest_version")
            .filter(|v| *v >= 1)
            .ok_or(KmsError::ResponseInvalid(
                "the answer has no latest version",
            ))?;
        let min_decryption_version = match data.get("min_decryption_version") {
            None => 1,
            Some(_) => u32_field(&data, "min_decryption_version")
                .filter(|v| *v >= 1)
                .ok_or(KmsError::ResponseInvalid(
                    "the minimum decryption version is not a number",
                ))?,
        };
        let enabled = match data.get("enabled") {
            None => true,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                return Err(KmsError::ResponseInvalid(
                    "the enabled flag is not a boolean",
                ));
            }
        };
        Ok(KeyInfo {
            latest_version,
            min_decryption_version,
            enabled,
        })
    }
}
