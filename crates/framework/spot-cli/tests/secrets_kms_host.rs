//! WP-SEC-03.T14: `[services.secrets] provider = "kms"`. The site's key provider is built from
//! `spot-secrets-kms` against an in-process fake Transit on a loopback address, through an egress
//! guard whose policy allows that one endpoint and nothing else. The fake is a copy of the T13
//! test fixture (a toy cipher that authenticates the context, so a wrong aad or an altered
//! ciphertext fails like the real service); no socket is opened.
//!
//! Only the command tests at the end need PostgreSQL (SPOT_TEST_PG_URL; skipped without it): they
//! load a real host root.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    unused_imports
)]

mod common;

/// The command registry, driven without a process (the technique of `commands_registry.rs`).
#[path = "../src/commands/mod.rs"]
mod commands;

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commands::{CommandCtx, Exit};
use sha2::{Digest, Sha256};
use spot_cli::services::Service;
use spot_cli::services::secrets::kms::{
    self, KmsHost, KmsSettings, KmsSite, REASONS, StatusFile, build_kms_provider_with, init_seed,
};
use spot_core::{Clock, ManualClock};
use spot_egress::{
    Authorized, EgressError, Guard, HttpRequest, InMemorySink, Resolver, Response, Sender,
};
use spot_http::health::ReadinessCheck;
use spot_secrets::SecretString;
use spot_secrets_kms::{Entropy, KmsError};
use time::macros::datetime;

const TOKEN: &str = "canary-token-s3cr3t-0001";
const KEY_NAME: &str = "spotkey";
const PLAIN: &str = "hunter2-plain-value-0001";
const AAD: &[u8] = b"site=acme;doctype=vault_note;field=token;name=A";
const SCOPE: &str = "vault_note.token";

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

// --- start, write, read, restart -------------------------------------------------------------

#[tokio::test]
async fn a_sealed_value_survives_a_restart_and_blind_indexes_do_not_change() {
    let rig = Rig::new();
    rig.init().await;
    let first = rig.host().await;
    let sealed = seal(&first, PLAIN).await;
    assert_eq!(open_text(&first, &sealed).await.unwrap(), PLAIN);
    let index = first
        .vault()
        .blind_index(SCOPE, b"lookup-value")
        .await
        .unwrap();
    drop(first);

    // A restart: a new host over the same seed file, an empty key cache.
    let second = rig.host().await;
    assert_eq!(open_text(&second, &sealed).await.unwrap(), PLAIN);
    assert_eq!(
        second
            .vault()
            .blind_index(SCOPE, b"lookup-value")
            .await
            .unwrap(),
        index
    );
    // The aad binds the value to its place.
    assert!(
        second
            .vault()
            .open(&sealed, b"another place")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn init_never_overwrites_an_existing_seed() {
    let rig = Rig::new();
    rig.init().await;
    let again = init_seed(
        &rig.settings,
        &rig.site,
        rig.guard(),
        rig.sender(),
        rig.clock(),
        entropy(),
    )
    .await;
    assert!(refusal(again).contains("secrets_kms_host.seed_exists"));
}

// --- the KMS goes away -----------------------------------------------------------------------

#[tokio::test]
async fn a_revoked_token_serves_from_cache_until_the_ttl_then_refuses_and_readiness_is_degraded() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.host().await;
    let sealed = seal(&host, PLAIN).await;
    assert_eq!(open_text(&host, &sealed).await.unwrap(), PLAIN);
    assert!(host.kms().cache_len() >= 1);
    let readiness = host.readiness();
    assert_eq!(readiness.check().await.detail, "");

    rig.fake.revoke_token();
    rig.clock.advance(Duration::from_secs(100));
    host.tick().await;
    // Degraded, not stopped: still ready, with the time left before refusals begin.
    let during = readiness.check().await;
    assert!(during.ok);
    assert!(during.detail.starts_with("degraded"), "{}", during.detail);
    assert!(
        during.detail.contains("200 more seconds"),
        "{}",
        during.detail
    );
    // The cached data key still serves inside the TTL.
    assert_eq!(open_text(&host, &sealed).await.unwrap(), PLAIN);

    // Past the TTL the cache has expired and the KMS refuses: a refusal, not a plaintext.
    rig.clock.advance(Duration::from_secs(201));
    let refused = open_text(&host, &sealed).await.unwrap_err();
    assert!(
        refused.to_string().contains("secrets_kms.rejected"),
        "{refused}"
    );
    assert!(
        host.vault()
            .seal(&SecretString::from("new"), AAD)
            .await
            .is_err()
    );
    let after = readiness.check().await;
    assert!(after.ok, "the site stays up: degraded, never a stop");
    assert!(after.detail.contains("being refused"), "{}", after.detail);
    // Refusals are counted under the closed reason set.
    let counts = host.state().refused_counts();
    assert!(counts.get("rejected").copied().unwrap_or(0) >= 2);
    assert!(counts.keys().all(|reason| REASONS.contains(reason)));
    assert_no_secret(&refused.to_string());
    assert_no_secret(&after.detail);

    // The token is valid again: the next probe clears the degraded state.
    rig.fake.set_token(TOKEN);
    rig.clock.advance(Duration::from_secs(61));
    host.tick().await;
    assert_eq!(readiness.check().await.detail, "");
    assert_eq!(open_text(&host, &sealed).await.unwrap(), PLAIN);
}

#[tokio::test]
async fn a_disabled_key_refuses_to_start_and_stops_a_running_check() {
    let rig = Rig::new();
    rig.init().await;
    let running = rig.host().await;
    rig.fake.disable();
    let again = running.startup_check().await;
    assert!(refusal(again).contains("secrets_kms_host.start_failed"));
    let fresh = rig.build_with(rig.guard()).await;
    let text = refusal(fresh);
    assert!(text.contains("secrets_kms_host.start_failed"), "{text}");
    assert!(text.contains(KEY_NAME), "{text}");
    assert_no_secret(&text);
}

// --- configuration and files -----------------------------------------------------------------

#[tokio::test]
async fn a_kms_host_the_policy_does_not_allow_refuses_start_and_names_the_entry() {
    let rig = Rig::new();
    rig.init().await;
    let other = guard_allowing("other.test", &rig.clock);
    let text = refusal(rig.build_with(other).await);
    assert!(
        text.contains("secrets_kms_host.egress_not_allowed"),
        "{text}"
    );
    assert!(text.contains("[[tenants.acme.allow]]"), "{text}");
    assert!(text.contains("host = \"kms.test\""), "{text}");
    assert!(text.contains("ports = [8200]"), "{text}");
    assert_no_secret(&text);
}

#[cfg(unix)]
#[tokio::test]
async fn a_world_readable_token_or_seed_refuses_start_naming_the_file_and_no_secret() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    rig.init().await;

    let token = rig.site.token_file.clone();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let text = refusal(rig.build_with(rig.guard()).await);
    assert!(text.contains("secrets_kms_host.file_unsafe"), "{text}");
    assert!(text.contains(&token.display().to_string()), "{text}");
    assert_no_secret(&text);
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();

    let seed = rig.site.seed_file.clone();
    std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o640)).unwrap();
    let text = refusal(rig.build_with(rig.guard()).await);
    assert!(text.contains("secrets_kms_host.file_unsafe"), "{text}");
    assert!(text.contains(&seed.display().to_string()), "{text}");
    std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o600)).unwrap();

    // Both private again: it starts.
    rig.build_with(rig.guard()).await.unwrap();
}

#[tokio::test]
async fn a_missing_token_or_seed_refuses_start_naming_the_file() {
    let rig = Rig::new();
    // No seed file yet: the server never makes one.
    let text = refusal(rig.build_with(rig.guard()).await);
    assert!(text.contains("secrets_kms_host.file_missing"), "{text}");
    assert!(
        text.contains(&rig.site.seed_file.display().to_string()),
        "{text}"
    );

    rig.init().await;
    std::fs::remove_file(&rig.site.token_file).unwrap();
    let text = refusal(rig.build_with(rig.guard()).await);
    assert!(text.contains("secrets_kms_host.file_missing"), "{text}");
    assert!(
        text.contains(&rig.site.token_file.display().to_string()),
        "{text}"
    );
}

#[test]
fn the_table_refuses_unknown_keys_and_bad_numbers_without_quoting_values() {
    let parse = |text: &str| KmsSettings::from_value(&toml::from_str::<toml::Value>(text).unwrap());
    let base = "address = \"https://kms.test:8200\"\nkey_name = \"k\"\ntoken_file = \"/t\"\nseed_file = \"/s\"\n";
    assert!(parse(base).is_ok());
    let text = refusal(parse(&format!("{base}bogus = \"s3cr3t-value\"\n")));
    assert!(text.contains("secrets_kms_host.settings_invalid"), "{text}");
    assert!(text.contains("unknown field"), "{text}");
    assert!(!text.contains("s3cr3t-value"), "{text}");
    let text = refusal(parse(&format!("{base}cache_ttl_seconds = 0\n")));
    assert!(text.contains("cache_ttl_seconds"), "{text}");
    let text = refusal(parse(&format!("{base}refresh_interval_seconds = 99999\n")));
    assert!(text.contains("refresh_interval_seconds"), "{text}");
    let text = refusal(parse("address = \"https://kms.test\"\nkey_name = \"k\"\n"));
    assert!(text.contains("missing field"), "{text}");
    // A relative path or one inside the site folders is refused as a path.
    let mut relative = parse(base).unwrap();
    relative.token_file = PathBuf::from("kms.token");
    let text = refusal(relative.for_site(Path::new("/srv/spot"), "acme", 1));
    assert!(text.contains("secrets_kms_host.path_refused"), "{text}");
    let mut inside = parse(base).unwrap();
    inside.seed_file = PathBuf::from("/srv/spot/sites/acme/seed.json");
    let text = refusal(inside.for_site(Path::new("/srv/spot"), "acme", 1));
    assert!(text.contains("secrets_kms_host.path_refused"), "{text}");
    // Several sites must not share one seed file.
    let text = refusal(
        parse(base)
            .unwrap()
            .for_site(Path::new("/srv/spot"), "acme", 2),
    );
    assert!(text.contains("secrets_kms_host.path_refused"), "{text}");
}

// --- rotation --------------------------------------------------------------------------------

#[tokio::test]
async fn the_sweep_after_a_kms_key_rotation_leaves_no_value_on_the_old_version() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.host().await;
    let vault = host.vault();
    let mut stored = Vec::new();
    for i in 0..5 {
        stored.push(seal(&host, &format!("{PLAIN}-{i}")).await);
    }
    assert_eq!(vault.current_key_version(), 1);
    for sealed in &stored {
        assert!(!vault.needs_rewrap(sealed).unwrap());
    }

    // Step 1: the KMS key gets a new version; the provider follows at the next refresh.
    rig.fake.rotate();
    host.kms().refresh().await.unwrap();
    assert_eq!(vault.current_key_version(), 2);
    for sealed in &stored {
        assert!(vault.needs_rewrap(sealed).unwrap());
    }
    // Step 2: the seed's wrapping moves.
    assert_eq!(host.rewrap_seed().await.unwrap(), 2);
    // Step 3: the sweep (the `rotate-kek` loop over stored values).
    let mut moved = Vec::new();
    for sealed in &stored {
        moved.push(vault.rewrap(sealed, AAD).await.unwrap());
    }
    let on_old = moved
        .iter()
        .filter(|s| vault.needs_rewrap(s).unwrap())
        .count();
    assert_eq!(on_old, 0, "no row is left on the old version");

    // Step 4: the old version is retired in the KMS; a fresh host (an empty cache) still reads
    // every swept value, and a value that was not swept is refused.
    let unswept = stored[0].clone();
    rig.fake.set_min_decryption(2);
    drop(host);
    let restarted = rig.host().await;
    for (i, sealed) in moved.iter().enumerate() {
        assert_eq!(
            open_text(&restarted, sealed).await.unwrap(),
            format!("{PLAIN}-{i}")
        );
    }
    let refused = open_text(&restarted, &unswept).await.unwrap_err();
    assert!(
        refused.to_string().contains("secrets_kms.version_retired"),
        "{refused}"
    );
    // The seed envelope moved too: the restart read it under version 2.
}

// --- the operator channel --------------------------------------------------------------------

#[tokio::test]
async fn the_service_reports_status_and_acts_on_a_flush_request() {
    let rig = Rig::new();
    rig.init().await;
    let host = rig.host().await;
    let sealed = seal(&host, PLAIN).await;
    open_text(&host, &sealed).await.unwrap();
    assert!(host.kms().cache_len() >= 1);

    let marker = kms::flush_marker_path(&rig.site.seed_file);
    write_private(&marker, b"");
    host.tick().await;
    assert!(
        !marker.exists(),
        "the service removes the marker it acted on"
    );
    assert_eq!(host.kms().cache_len(), 0);

    let text = std::fs::read_to_string(kms::status_path(&rig.site.seed_file)).unwrap();
    assert_no_secret(&text);
    assert!(
        !text.contains("kms.test"),
        "the address is not in the status file"
    );
    let status: StatusFile = serde_json::from_str(&text).unwrap();
    assert_eq!(status.key_name, KEY_NAME);
    assert_eq!(status.latest_version, 1);
    assert_eq!(status.min_decryption_version, 1);
    assert!(status.key_enabled);
    assert_eq!(status.cache_entries, 0);
    assert_eq!(status.health, "ok");
    assert_eq!(status.last_error, None);
    // The status file is private, like the files beside it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(kms::status_path(&rig.site.seed_file))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
    // The next unseal asks the KMS again and works.
    assert_eq!(open_text(&host, &sealed).await.unwrap(), PLAIN);
}

// --- the command -----------------------------------------------------------------------------

#[test]
fn the_command_registers_and_parses() {
    let registration = commands::COMMANDS
        .iter()
        .find(|r| r.name == "secrets-kms")
        .expect("the secrets-kms command registers itself");
    for args in [
        vec!["secrets-kms", "init", "--site", "acme"],
        vec!["secrets-kms", "init", "--site", "acme", "--apply"],
        vec!["secrets-kms", "status", "--site", "acme"],
        vec!["secrets-kms", "flush-cache", "--site", "acme", "--apply"],
        vec!["secrets-kms", "rewrap-seed", "--site", "acme"],
    ] {
        (registration.command)()
            .try_get_matches_from(args)
            .expect("the arguments parse");
    }
    assert!(
        (registration.command)()
            .try_get_matches_from(["secrets-kms", "status"])
            .is_err(),
        "--site is required"
    );
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

async fn kms_cmd(root: &Path, args: &[&str]) -> (Exit, String) {
    let (out, err) = (Sink::default(), Sink::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-13 09:00:00 UTC)));
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        clock,
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let registration = commands::COMMANDS
        .iter()
        .find(|r| r.name == "secrets-kms")
        .expect("the secrets-kms command registers itself");
    let mut argv = vec!["secrets-kms"];
    argv.extend_from_slice(args);
    let matches = (registration.command)()
        .try_get_matches_from(argv)
        .expect("the arguments parse");
    let exit = match (registration.run)(&matches, &mut ctx).await {
        Ok(exit) => exit,
        Err(e) => {
            let _ = writeln!(ctx.err(), "{e}");
            Exit::Failed
        }
    };
    (exit, format!("{}{}", out.text(), err.text()))
}

/// Real PostgreSQL (skipped without SPOT_TEST_PG_URL): a host root whose `common.toml` names the
/// KMS provider, to drive `status` and `flush-cache` as an operator would.
#[tokio::test]
async fn status_and_flush_cache_read_and_write_the_files_beside_the_seed() {
    let dir = std::env::temp_dir().join(format!("spot_kms_t14_cmd_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let services = format!(
        "[services.secrets]\nprovider = \"kms\"\n[services.secrets.kms]\naddress = \"https://kms.test:8200\"\nkey_name = \"{KEY_NAME}\"\ntoken_file = {:?}\nseed_file = {:?}\n",
        dir.join("kms.token").display().to_string(),
        format!("{}/{{site}}.seed.json", dir.display()),
    );
    let Some(env) = common::Env::new(&services).await else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    env.site();
    let seed = dir.join(format!("{}.seed.json", env.site));
    write_private(&seed, b"{}");
    let root = env.root.0.clone();

    // Nothing reported yet.
    let (exit, text) = kms_cmd(&root, &["status", "--site", &env.site]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(text.contains("present"), "{text}");
    assert!(text.contains("no status reported"), "{text}");

    // A status as the running server writes it.
    let status = StatusFile {
        updated_unix: 0,
        key_name: KEY_NAME.into(),
        latest_version: 3,
        min_decryption_version: 2,
        key_enabled: true,
        cache_entries: 4,
        cache_age_seconds: 17,
        health: "degraded".into(),
        seconds_left: Some(120),
        last_error: Some("secrets_kms.unreachable".into()),
        refused: [("unreachable".to_owned(), 2u64)].into(),
    };
    write_private(
        &kms::status_path(&seed),
        &serde_json::to_vec(&status).unwrap(),
    );
    let (exit, text) = kms_cmd(&root, &["status", "--site", &env.site]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    for needle in [
        "latest version: 3",
        "minimum decryption version: 2",
        "4 data keys",
        "degraded (120 seconds before refusals begin)",
        "secrets_kms.unreachable",
        "refused (unreachable): 2",
    ] {
        assert!(text.contains(needle), "{needle} missing from {text}");
    }
    assert_no_secret(&text);

    // flush-cache: the plan first, the marker only with --apply.
    let marker = kms::flush_marker_path(&seed);
    let (exit, text) = kms_cmd(&root, &["flush-cache", "--site", &env.site]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(!marker.exists());
    let (exit, text) = kms_cmd(&root, &["flush-cache", "--site", &env.site, "--apply"]).await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert!(marker.exists());

    // An unknown site is a usage error.
    let (exit, _) = kms_cmd(&root, &["status", "--site", "no-such-site"]).await;
    assert_eq!(exit, Exit::Usage);
    let _ = std::fs::remove_dir_all(&dir);
}
