//! WP-SEC-03.T16: the secrets service picks its key provider from `[services.secrets]`. The
//! local key file keeps working as before; `provider = "kms"` builds the site's provider from
//! `spot-secrets-kms` through the shared egress guard, behind the same `SecretsHost`. The KMS is
//! an in-process fake Transit (a copy of the T14 fixture); no socket is opened and no database is
//! needed.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    unused_imports
)]

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use spot_cli::services::Service;
use spot_cli::services::secrets::kms::{
    KmsHost, KmsSettings, KmsSite, build_kms_provider_with, init_seed,
};
use spot_cli::services::secrets::{
    self as svc, Runtime, SecretsHost, Settings, add_version, codes, init_key_file, open_provider,
};
use spot_core::{Clock, ManualClock};
use spot_egress::{
    Authorized, EgressError, Guard, HttpRequest, InMemorySink, Resolver, Response, Sender,
};
use spot_secrets::SecretString;
use spot_secrets_kms::{Entropy, KmsError};
use time::macros::datetime;

const TOKEN: &str = "canary-token-s3cr3t-0001";
const KEY_NAME: &str = "spotkey";
const PLAIN: &str = "hunter2-plain-value-0001";
const AAD: &[u8] = b"site=acme;doctype=vault_note;field=token;name=A";

// --- a minimal base64 (the Transit wire form) ------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.trim_end_matches('=').bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// --- the fake Transit ------------------------------------------------------------------------

struct FakeState {
    latest: u32,
    min_decryption: u32,
    disabled: bool,
    token: String,
}

struct FakeTransit {
    state: Mutex<FakeState>,
}

impl FakeTransit {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeState {
                latest: 1,
                min_decryption: 1,
                disabled: false,
                token: TOKEN.to_owned(),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }

    /// A transit key rotation: new wraps use the next version.
    fn rotate(&self) {
        self.lock().latest += 1;
    }

    fn set_min_decryption(&self, version: u32) {
        self.lock().min_decryption = version;
    }

    fn disable(&self) {
        self.lock().disabled = true;
    }

    /// Every call answers 403 from now on.
    fn revoke_token(&self) {
        self.lock().token = "some-other-token".to_owned();
    }

    fn set_token(&self, token: &str) {
        self.lock().token = token.to_owned();
    }
}

fn version_key(version: u32) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(b"fake transit master");
    h.update(version.to_be_bytes());
    h.finalize().to_vec()
}

fn keystream(version: u32) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(version_key(version));
    h.update(b"stream");
    h.finalize().to_vec()
}

fn tag(version: u32, context: &[u8], body: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(version_key(version));
    h.update((context.len() as u64).to_be_bytes());
    h.update(context);
    h.update(body);
    h.finalize()[..16].to_vec()
}

fn reply(status: u16, body: serde_json::Value) -> Response {
    Response {
        status: http::StatusCode::from_u16(status).unwrap(),
        headers: http::HeaderMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
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
        let state = self.lock();
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
        if parts.len() != 2 || parts[1] != KEY_NAME {
            return Ok(reply(404, serde_json::json!({"errors": ["key not found"]})));
        }
        let body: serde_json::Value = match &request.body {
            Some(bytes) if !bytes.is_empty() => serde_json::from_slice(bytes).unwrap(),
            _ => serde_json::Value::Null,
        };
        let context = body
            .get("context")
            .and_then(|v| v.as_str())
            .map(|c| b64_decode(c).unwrap())
            .unwrap_or_default();
        match parts[0] {
            "keys" => Ok(reply(
                200,
                serde_json::json!({"data": {
                    "latest_version": state.latest,
                    "min_decryption_version": state.min_decryption,
                    "enabled": !state.disabled,
                    "type": "aes256-gcm96",
                }}),
            )),
            "encrypt" => {
                if state.disabled {
                    return Ok(denied());
                }
                let plain = b64_decode(body["plaintext"].as_str().unwrap()).unwrap();
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
                        "ciphertext": format!("vault:v{version}:{}", b64_encode(&sealed)),
                        "key_version": version,
                    }}),
                ))
            }
            "decrypt" => {
                if state.disabled {
                    return Ok(denied());
                }
                let text = body["ciphertext"].as_str().unwrap();
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
                let Some(raw) = b64_decode(b64) else {
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
                    serde_json::json!({"data": {"plaintext": b64_encode(&plain)}}),
                ))
            }
            _ => Ok(reply(404, serde_json::json!({"errors": ["no handler"]}))),
        }
    }
}

// --- the rig ---------------------------------------------------------------------------------

/// Resolves every host to the loopback address (the fake has no network).
struct Loopback;

#[async_trait::async_trait]
impl Resolver for Loopback {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        Ok(vec![IpAddr::from([127, 0, 0, 1])])
    }
}

/// Deterministic entropy.
struct FixedEntropy(AtomicU8);

impl Entropy for FixedEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), KmsError> {
        let base = self.0.load(Ordering::SeqCst);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = base.wrapping_add(i as u8);
        }
        Ok(())
    }
}

fn entropy() -> Arc<dyn Entropy> {
    Arc::new(FixedEntropy(AtomicU8::new(7)))
}

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// An egress guard whose policy lets site `acme` reach `host` on port 8200 and nothing else.
fn guard_allowing(host: &str, clock: &Arc<ManualClock>) -> Arc<Guard> {
    let policy = spot_egress::EgressPolicy::from_toml(&format!(
        "[tenants.acme]\n[[tenants.acme.allow]]\nscheme = \"https\"\nhost = \"{host}\"\nports = [8200]\npurpose = \"connector\"\nallow_private = [\"127.0.0.1\"]\n"
    ))
    .unwrap();
    Arc::new(Guard::new(
        policy,
        Arc::new(Loopback),
        Arc::new(InMemorySink::new()),
        Arc::clone(clock) as Arc<dyn Clock>,
    ))
}

struct Rig {
    dir: PathBuf,
    fake: Arc<FakeTransit>,
    clock: Arc<ManualClock>,
    settings: KmsSettings,
    site: KmsSite,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    fn new() -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "spot_kms_t14_{}_{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join("kms.token");
        write_private(&token, TOKEN.as_bytes());
        let seed = dir.join("acme.seed.json");
        let settings = settings_text(&token, &seed, "");
        let site = settings.for_site(&dir, "acme", 1).unwrap();
        Rig {
            dir,
            fake: FakeTransit::new(),
            clock: Arc::new(ManualClock::new(datetime!(2026-11-13 09:00:00 UTC))),
            settings,
            site,
        }
    }

    fn guard(&self) -> Arc<Guard> {
        guard_allowing("kms.test", &self.clock)
    }

    fn sender(&self) -> Arc<dyn Sender> {
        Arc::clone(&self.fake) as Arc<dyn Sender>
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock) as Arc<dyn Clock>
    }

    /// The first start of the site: the seed file is made.
    async fn init(&self) {
        init_seed(
            &self.settings,
            &self.site,
            self.guard(),
            self.sender(),
            self.clock(),
            entropy(),
        )
        .await
        .unwrap();
    }

    async fn build_with(&self, guard: Arc<Guard>) -> spot_cli::Result<Arc<KmsHost>> {
        build_kms_provider_with(
            &self.settings,
            &self.site,
            guard,
            self.sender(),
            self.clock(),
            entropy(),
        )
        .await
    }

    async fn host(&self) -> Arc<KmsHost> {
        let host = self.build_with(self.guard()).await.unwrap();
        host.service().check().await.unwrap();
        host
    }
}

fn settings_text(token: &Path, seed: &Path, extra: &str) -> KmsSettings {
    let text = format!(
        "address = \"https://kms.test:8200\"\nkey_name = \"{KEY_NAME}\"\ntoken_file = {:?}\nseed_file = {:?}\ncache_ttl_seconds = 300\nrefresh_interval_seconds = 60\n{extra}",
        token.display().to_string(),
        seed.display().to_string(),
    );
    KmsSettings::from_value(&toml::from_str::<toml::Value>(&text).unwrap()).unwrap()
}

/// The text of a refusal; the test fails when there was none.
fn refusal<T>(result: spot_cli::Result<T>) -> String {
    match result {
        Err(e) => e.0,
        Ok(_) => panic!("expected a refusal"),
    }
}

/// Everything a refusal, a readiness line or a status file may say must be free of these.
fn assert_no_secret(text: &str) {
    for secret in [TOKEN, PLAIN] {
        assert!(!text.contains(secret), "a secret leaked into: {text}");
    }
}

async fn seal(host: &KmsHost, plain: &str) -> Vec<u8> {
    host.vault()
        .seal(&SecretString::from(plain), AAD)
        .await
        .unwrap()
}

async fn open_text(host: &KmsHost, sealed: &[u8]) -> Result<String, spot_secrets::SecretError> {
    host.vault()
        .open(sealed, AAD)
        .await
        .map(|s| s.expose().to_owned())
}

// --- the wiring ------------------------------------------------------------------------------

/// `[services.secrets]` for the rig's KMS, as the operator writes it.
fn kms_table(rig: &Rig) -> String {
    format!(
        "provider = \"kms\"\n[kms]\naddress = \"https://kms.test:8200\"\nkey_name = \"{KEY_NAME}\"\ntoken_file = {:?}\nseed_file = {:?}\n",
        rig.settings_token().display().to_string(),
        rig.site.seed_file.display().to_string(),
    )
}

fn parse(text: &str) -> Settings {
    toml::from_str(text).unwrap()
}

impl Rig {
    fn settings_token(&self) -> PathBuf {
        self.site.token_file.clone()
    }

    fn runtime(&self, guard: Arc<Guard>) -> Runtime {
        Runtime {
            guard: Some(guard),
            sender: Some(self.sender()),
            clock: self.clock(),
            entropy: entropy(),
        }
    }

    async fn open(&self, guard: Arc<Guard>) -> spot_cli::Result<Arc<SecretsHost>> {
        let settings = parse(&kms_table(self));
        open_provider(&settings, &self.dir, "acme", 1, 0, &self.runtime(guard)).await
    }
}

#[test]
fn settings_accept_the_kms_table_and_refuse_what_does_not_fit() {
    let rig = Rig::new();
    let kms = parse(&kms_table(&rig));
    assert_eq!(kms.provider, "kms");
    kms.check().unwrap();
    assert_eq!(kms.kms.as_ref().unwrap().key_name, KEY_NAME);

    // Unknown keys inside the kms table are refused by name.
    let text = format!("{}surprise = 1\n", kms_table(&rig));
    let error = toml::from_str::<Settings>(&text).unwrap_err();
    assert!(error.message().starts_with("unknown field"), "{error}");

    // `kms` provider without its table, a kms table under `local`, and an unknown provider.
    let missing = parse("provider = \"kms\"");
    assert!(
        missing
            .check()
            .unwrap_err()
            .0
            .starts_with(codes::SETTINGS_INVALID)
    );
    let wrong = parse(&kms_table(&rig).replacen("provider = \"kms\"", "provider = \"local\"", 1));
    assert!(
        wrong
            .check()
            .unwrap_err()
            .0
            .starts_with(codes::SETTINGS_INVALID)
    );
    let other = parse("provider = \"hsm\"");
    assert!(
        other
            .check()
            .unwrap_err()
            .0
            .starts_with(codes::SETTINGS_INVALID)
    );
    // The default is the local key file, unchanged.
    parse("kek_file = \"/k\"").check().unwrap();
}

#[tokio::test]
async fn the_kms_provider_seals_opens_and_verifies_through_the_site_host() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.open(rig.guard()).await.unwrap();
    assert_eq!(host.provider_name(), "kms");
    assert!(host.kms_host().is_some());
    assert_eq!(host.path(), rig.site.seed_file.as_path());

    // Seal and open through the vault the engine gets.
    let vault = host.vault();
    let sealed = vault.seal(&SecretString::from(PLAIN), AAD).await.unwrap();
    let opened = vault.open(&sealed, AAD).await.unwrap();
    assert_eq!(opened.expose(), PLAIN);

    // The key-version view `spot secrets status` and `verify_values` use is the provider's.
    let versions = host.versions().await;
    assert_eq!(versions.current, 1);
    assert!(versions.held.contains(&1));
    assert!(host.keyring().current().version == host.current_version());
    host.canary().await.unwrap();

    // Rotation: the KMS key gets a version; the provider follows; the rewrap moves the value.
    rig.fake.rotate();
    host.kms_host().unwrap().kms().refresh().await.unwrap();
    assert_eq!(host.current_version(), 2);
    assert!(vault.needs_rewrap(&sealed).unwrap());
    let moved = vault.rewrap(&sealed, AAD).await.unwrap();
    assert!(!vault.needs_rewrap(&moved).unwrap());
    assert_eq!(vault.open(&moved, AAD).await.unwrap().expose(), PLAIN);
}

#[tokio::test]
async fn key_file_operations_are_refused_under_kms_with_the_catalogued_code() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.open(rig.guard()).await.unwrap();
    let refused = host.local().err().unwrap().0;
    assert!(
        refused.starts_with(codes::PROVIDER_UNSUPPORTED),
        "{refused}"
    );
    assert!(
        svc::add_version(&host)
            .await
            .unwrap_err()
            .0
            .starts_with(codes::PROVIDER_UNSUPPORTED)
    );
    assert!(
        svc::retire_version(&host, 1, 0)
            .await
            .unwrap_err()
            .0
            .starts_with(codes::PROVIDER_UNSUPPORTED)
    );
}

#[tokio::test]
async fn a_kms_host_the_site_policy_denies_stops_the_start_with_the_entry_to_add() {
    let rig = Rig::new();
    rig.init().await;
    let other = guard_allowing("other.test", &rig.clock);
    let text = match rig.open(other).await {
        Err(e) => e.0,
        Ok(_) => panic!("a denied KMS host must stop the start"),
    };
    assert!(
        text.contains("secrets_kms_host.egress_not_allowed"),
        "{text}"
    );
    assert!(text.contains("host = \"kms.test\""), "{text}");
    assert_no_secret(&text);
}

#[tokio::test]
async fn no_marker_secret_reaches_a_debug_or_a_refusal() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.open(rig.guard()).await.unwrap();
    assert_no_secret(&format!("{host:?}"));
    // A missing token file is refused naming the file and never its content.
    std::fs::remove_file(&rig.site.token_file).unwrap();
    let text = match rig.open(rig.guard()).await {
        Err(e) => e.0,
        Ok(_) => panic!("a missing token file must stop the start"),
    };
    assert!(text.contains("kms.token"), "{text}");
    assert_no_secret(&text);
}

#[tokio::test]
async fn the_local_provider_is_unchanged() {
    let dir = std::env::temp_dir().join(format!(
        "spot_kms_t16_local_{}_{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let kek = dir.join("acme.kek.json");
    init_key_file(&kek, "acme").await.unwrap();
    let settings = parse(&format!("kek_file = {:?}\n", kek.display().to_string()));
    let host = open_provider(&settings, &dir, "acme", 1, 0, &Runtime::production())
        .await
        .unwrap();
    assert_eq!(host.provider_name(), "local");
    assert!(host.kms_host().is_none());
    assert_eq!(host.path(), kek.as_path());
    let vault = host.vault();
    let sealed = vault.seal(&SecretString::from(PLAIN), AAD).await.unwrap();
    assert_eq!(vault.open(&sealed, AAD).await.unwrap().expose(), PLAIN);
    // Rotation of the key file still works through the same host.
    assert_eq!(add_version(&host).await.unwrap(), 2);
    assert_eq!(host.versions().await.current, 2);
    host.local().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
