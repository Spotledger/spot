//! An in-process fake Vault Transit service and the helpers the contract tests share.
//!
//! `FakeTransit` is a `Sender`: it answers the same paths, headers and JSON shapes as Transit,
//! with a toy cipher (a keystream and a MAC per key version) that authenticates the context, so
//! a wrong aad or an altered ciphertext fails like the real service. No socket is opened.
#![allow(dead_code, missing_docs, clippy::expect_used)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use http::{HeaderMap, StatusCode};
use sha2::Sha256;
use spot_core::ManualClock;
use spot_egress::{Authorized, EgressError, HttpRequest, Response, Sender};
use spot_secrets_kms::{
    EgressGate, Entropy, KmsError, KmsKeyProvider, ProviderConfig, TransitClient, TransitConfig,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

pub const TOKEN: &str = "canary-token-s3cr3t-0001";
pub const KEY_NAME: &str = "spotkey";
pub const AAD: &[u8] = b"site=acme;doctype=Email Account;field=password;name=Sales";

type HmacSha256 = Hmac<Sha256>;

pub struct FakeState {
    pub latest: u32,
    pub min_decryption: u32,
    pub disabled: bool,
    pub token: String,
    pub encrypts: usize,
    pub decrypts: usize,
    pub infos: usize,
    /// Everything the fake ever received, for the leak assertions.
    pub seen_bodies: Vec<String>,
}

pub struct FakeTransit {
    /// The key name this fake serves (any other name answers 404).
    pub name: String,
    pub state: Mutex<FakeState>,
}

impl FakeTransit {
    pub fn new() -> Arc<Self> {
        Self::named(KEY_NAME)
    }

    pub fn named(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            state: Mutex::new(FakeState {
                latest: 1,
                min_decryption: 1,
                disabled: false,
                token: TOKEN.to_owned(),
                encrypts: 0,
                decrypts: 0,
                infos: 0,
                seen_bodies: Vec::new(),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state")
    }

    /// A transit key rotation: new wraps use the next version.
    pub fn rotate(&self) {
        self.lock().latest += 1;
    }

    pub fn set_min_decryption(&self, version: u32) {
        self.lock().min_decryption = version;
    }

    /// The key is disabled: encrypt and decrypt answer 403 and the key read says so.
    pub fn disable(&self) {
        self.lock().disabled = true;
    }

    /// The token is revoked: every call answers 403.
    pub fn revoke_token(&self) {
        self.lock().token = "some-other-token".to_owned();
    }

    /// The token the fake accepts from now on.
    pub fn set_token(&self, token: &str) {
        self.lock().token = token.to_owned();
    }

    pub fn decrypts(&self) -> usize {
        self.lock().decrypts
    }

    pub fn encrypts(&self) -> usize {
        self.lock().encrypts
    }

    pub fn seen(&self) -> String {
        self.lock().seen_bodies.join("\n")
    }
}

fn version_key(version: u32) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(b"fake transit master").expect("hmac");
    mac.update(&version.to_be_bytes());
    mac.finalize().into_bytes().into()
}

fn keystream(version: u32) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(&version_key(version)).expect("hmac");
    mac.update(b"stream");
    mac.finalize().into_bytes().into()
}

fn tag(version: u32, context: &[u8], body: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(&version_key(version)).expect("hmac");
    mac.update(&(context.len() as u64).to_be_bytes());
    mac.update(context);
    mac.update(body);
    mac.finalize().into_bytes()[..16].to_vec()
}

fn reply(status: u16, body: serde_json::Value) -> Response {
    Response {
        status: StatusCode::from_u16(status).expect("status"),
        headers: HeaderMap::new(),
        body: serde_json::to_vec(&body).expect("json"),
    }
}

fn denied() -> Response {
    reply(403, serde_json::json!({"errors": ["permission denied"]}))
}

#[async_trait::async_trait]
impl Sender for FakeTransit {
    async fn send_request(
        &self,
        request: &HttpRequest,
        _authorized: &Authorized,
        _max_body: u64,
    ) -> Result<Response, EgressError> {
        let mut state = self.lock();
        let body_text = request
            .body
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        state.seen_bodies.push(body_text.clone());
        let token = request
            .headers
            .get("x-vault-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if token != state.token {
            return Ok(denied());
        }
        let path = request.url.path().to_owned();
        let Some(rest) = path.strip_prefix("/v1/transit/") else {
            return Ok(reply(404, serde_json::json!({"errors": ["no handler"]})));
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 2 || parts[1] != self.name {
            return Ok(reply(404, serde_json::json!({"errors": ["key not found"]})));
        }
        let body: serde_json::Value = if body_text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&body_text).expect("request json")
        };
        let context = body
            .get("context")
            .and_then(|v| v.as_str())
            .map(|c| STANDARD.decode(c).expect("context b64"))
            .unwrap_or_default();
        match parts[0] {
            "keys" => {
                state.infos += 1;
                Ok(reply(
                    200,
                    serde_json::json!({"data": {
                        "latest_version": state.latest,
                        "min_decryption_version": state.min_decryption,
                        "enabled": !state.disabled,
                        "type": "aes256-gcm96",
                    }}),
                ))
            }
            "encrypt" => {
                state.encrypts += 1;
                if state.disabled {
                    return Ok(denied());
                }
                let plain = STANDARD
                    .decode(body["plaintext"].as_str().expect("plaintext"))
                    .expect("plaintext b64");
                let version = state.latest;
                let stream = keystream(version);
                let mut sealed: Vec<u8> = plain
                    .iter()
                    .enumerate()
                    .map(|(i, b)| b ^ stream[i % 32])
                    .collect();
                let mac = tag(version, &context, &sealed);
                sealed.extend_from_slice(&mac);
                Ok(reply(
                    200,
                    serde_json::json!({"data": {
                        "ciphertext": format!("vault:v{version}:{}", STANDARD.encode(sealed)),
                        "key_version": version,
                    }}),
                ))
            }
            "decrypt" => {
                state.decrypts += 1;
                if state.disabled {
                    return Ok(denied());
                }
                let text = body["ciphertext"].as_str().expect("ciphertext");
                let bad = || {
                    reply(
                        400,
                        serde_json::json!({"errors": ["cipher: message authentication failed"]}),
                    )
                };
                let Some(rest) = text.strip_prefix("vault:v") else {
                    return Ok(bad());
                };
                let Some((digits, b64)) = rest.split_once(':') else {
                    return Ok(bad());
                };
                let Ok(version) = digits.parse::<u32>() else {
                    return Ok(bad());
                };
                if version == 0 || version > state.latest {
                    return Ok(bad());
                }
                if version < state.min_decryption {
                    return Ok(reply(
                        400,
                        serde_json::json!({"errors": ["ciphertext or signature version is disallowed by policy (too old)"]}),
                    ));
                }
                let Ok(raw) = STANDARD.decode(b64) else {
                    return Ok(bad());
                };
                if raw.len() < 16 {
                    return Ok(bad());
                }
                let (sealed, mac) = raw.split_at(raw.len() - 16);
                if tag(version, &context, sealed) != mac {
                    return Ok(bad());
                }
                let stream = keystream(version);
                let plain: Vec<u8> = sealed
                    .iter()
                    .enumerate()
                    .map(|(i, b)| b ^ stream[i % 32])
                    .collect();
                Ok(reply(
                    200,
                    serde_json::json!({"data": {"plaintext": STANDARD.encode(plain)}}),
                ))
            }
            _ => Ok(reply(404, serde_json::json!({"errors": ["no handler"]}))),
        }
    }
}

/// A gate that authorises every URL without resolving anything (the fake has no network).
pub struct OpenGate;

#[async_trait::async_trait]
impl EgressGate for OpenGate {
    async fn authorize(&self, url: &Url) -> Result<Authorized, EgressError> {
        Ok(Authorized {
            addrs: Vec::new(),
            original_host: url.host_str().unwrap_or_default().to_owned(),
        })
    }
}

/// Deterministic entropy: every byte is `fill`.
pub struct FixedEntropy(pub AtomicU8);

impl FixedEntropy {
    pub fn new(fill: u8) -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(fill)))
    }
}

impl Entropy for FixedEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), KmsError> {
        let base = self.0.load(Ordering::SeqCst);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = base.wrapping_add(i as u8);
        }
        Ok(())
    }
}

pub fn write_token(dir: &Path, token: &str) -> PathBuf {
    let path = dir.join("kms.token");
    write_private(&path, token.as_bytes());
    path
}

/// Writes `bytes` to `path` with mode 0600 (the seed and token files are checked for it).
pub fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

pub fn transit_config(token_file: PathBuf) -> TransitConfig {
    transit_config_named(token_file, KEY_NAME)
}

pub fn transit_config_named(token_file: PathBuf, key_name: &str) -> TransitConfig {
    TransitConfig {
        address: "https://kms.test:8200".into(),
        mount: "transit".into(),
        key_name: key_name.into(),
        token_file,
        namespace: None,
        request_timeout: Duration::from_secs(5),
    }
}

pub async fn client(fake: &Arc<FakeTransit>, dir: &Path) -> Arc<TransitClient> {
    let token_file = write_token(dir, TOKEN);
    Arc::new(
        TransitClient::open(
            transit_config_named(token_file, &fake.name),
            Arc::clone(fake) as Arc<dyn Sender>,
            Arc::new(OpenGate),
        )
        .await
        .expect("client"),
    )
}

pub fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH))
}

pub fn seed_path(dir: &Path) -> PathBuf {
    dir.join("kms-seed.json")
}

pub async fn open_with(
    fake: &Arc<FakeTransit>,
    dir: &Path,
    clock: &Arc<ManualClock>,
    entropy_fill: u8,
    config: impl FnOnce(&mut ProviderConfig),
) -> Result<KmsKeyProvider, spot_secrets::SecretError> {
    let client = client(fake, dir).await;
    let mut cfg = ProviderConfig::new(seed_path(dir));
    config(&mut cfg);
    KmsKeyProvider::open(
        client,
        cfg,
        Arc::clone(clock) as _,
        FixedEntropy::new(entropy_fill),
    )
    .await
}

pub async fn open(fake: &Arc<FakeTransit>, dir: &Path, clock: &Arc<ManualClock>) -> KmsKeyProvider {
    open_with(fake, dir, clock, 7, |_| {})
        .await
        .expect("open provider")
}
