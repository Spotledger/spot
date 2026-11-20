//! The customer-managed-key provider of the secrets service (WP-SEC-03.T14): `[services.secrets]`
//! with `provider = "kms"` builds the site's key provider from `spot-secrets-kms` instead of the
//! local key file. Operator guide: `docs/running-spot/secrets-kms.md`.
//!
//! [`build_kms_provider`] turns `[services.secrets.kms]` ([`KmsSettings`]) into a [`KmsHost`]: the
//! vetted token and seed files, an egress check of the KMS address against the site's shared
//! `Guard`, the transit client over a guarded sender, the key provider and the vault. Refusals are
//! counted as `spot_secrets_kms_refused_total{reason}` (closed set [`REASONS`]).
//!
//! **Level.** [`KmsService`] is required: its start-up check (key readable and enabled, canary
//! wrapped and unwrapped, blind-index key derived) stops the site on failure. After start the site
//! never stops because the KMS is away: [`KmsReadiness`] reports `degraded` (still ready) with the
//! seconds left before refusals begin.
//!
//! **Files.** The token and seed files must be regular files that grant nothing to group or
//! others. `spot secrets-kms init` makes the seed file; the server never does (a new seed would
//! silently change every blind index).
//!
//! **Operator channel.** The CLI is another process. The service writes `<seed_file>.status.json`
//! (counts, versions, reason codes only) at each probe and removes `<seed_file>.flush` after
//! emptying its key cache (`flush-cache` creates the marker).

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_document::secrets::SecretVault;
use spot_egress::{EgressConnector, EgressError, Guard, HttpSender, Purpose, Sender};
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_secrets::{
    KeyProvider, KeyRef, ProviderFuture, SecretBox, SecretBytes, SecretError, WrappedKey,
};
use spot_secrets_kms::{
    EgressGate, Entropy, GuardGate, KeyInfo, KmsClient, KmsKeyProvider, ProviderConfig,
    TransitClient, TransitConfig,
};
use tokio::sync::watch;
use url::Url;

use crate::services::{CheckFuture, Level, RunFuture, Service};
use crate::{Error, Result};

/// The service name readiness and logs use.
pub const SERVICE_NAME: &str = "secrets_kms";

/// How often the service looks for a flush request and whether a probe is due.
const TICK: Duration = Duration::from_secs(2);
/// The longest one KMS call may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest KMS address accepted.
const MAX_ADDRESS: usize = 2048;

/// The stable codes of this module (catalogue: `secrets_kms_host.toml`). A refusal is
/// `"<code>: <text>"`; the text names a file, a policy entry or a count, never a secret.
#[allow(missing_docs)]
pub mod codes {
    pub const SETTINGS_INVALID: &str = "secrets_kms_host.settings_invalid";
    pub const PATH_REFUSED: &str = "secrets_kms_host.path_refused";
    pub const FILE_MISSING: &str = "secrets_kms_host.file_missing";
    pub const FILE_UNUSABLE: &str = "secrets_kms_host.file_unusable";
    pub const FILE_UNSAFE: &str = "secrets_kms_host.file_unsafe";
    pub const EGRESS_NOT_ALLOWED: &str = "secrets_kms_host.egress_not_allowed";
    pub const START_FAILED: &str = "secrets_kms_host.start_failed";
    pub const CANARY_FAILED: &str = "secrets_kms_host.canary_failed";
    pub const SEED_EXISTS: &str = "secrets_kms_host.seed_exists";
    pub const REWRAP_FAILED: &str = "secrets_kms_host.rewrap_failed";
}

fn refuse(code: &str, text: impl std::fmt::Display) -> Error {
    Error(format!("{code}: {text}"))
}

/// `[services.secrets.kms]` of `common.toml`. `address` is `https` only; `token_file` and
/// `seed_file` are absolute and private to the service user (`{site}` is replaced; the seed file
/// needs it when the host has several sites; back the seed up, a lost seed loses every blind
/// index); `cache_ttl_seconds` is the revocation latency; `refresh_interval_seconds` is how often
/// the key's versions are re-read.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct KmsSettings {
    pub address: String,
    #[serde(default = "default_mount")]
    pub mount: String,
    pub key_name: String,
    pub token_file: PathBuf,
    pub seed_file: PathBuf,
    #[serde(default = "default_ttl")]
    pub cache_ttl_seconds: u64,
    #[serde(default = "default_refresh")]
    pub refresh_interval_seconds: u64,
}

#[rustfmt::skip]
fn default_mount() -> String { "transit".into() }

#[rustfmt::skip]
fn default_ttl() -> u64 { 300 }

#[rustfmt::skip]
fn default_refresh() -> u64 { 60 }

/// The site (also the tenant of the egress policy) and its resolved token and seed files.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct KmsSite {
    pub site: String,
    pub token_file: PathBuf,
    pub seed_file: PathBuf,
}

impl KmsSettings {
    /// Reads the table. The error names the key and never a value.
    pub fn from_value(table: &toml::Value) -> Result<KmsSettings> {
        match table.clone().try_into::<KmsSettings>() {
            Ok(settings) => {
                settings.validate()?;
                Ok(settings)
            }
            Err(e) => {
                let message = e.message();
                // `unknown field` and `missing field` quote key names only; anything else can
                // quote the value (a path an operator keeps private).
                let why = if message.starts_with("unknown field")
                    || message.starts_with("missing field")
                {
                    message.to_owned()
                } else {
                    "a value has the wrong type".to_owned()
                };
                Err(refuse(
                    codes::SETTINGS_INVALID,
                    format!("services.secrets.kms: {why}"),
                ))
            }
        }
    }

    /// The bounds of the numbers and lengths.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| {
            Err(refuse(
                codes::SETTINGS_INVALID,
                format!("services.secrets.kms.{what}"),
            ))
        };
        if self.address.is_empty() || self.address.len() > MAX_ADDRESS {
            return bad("address is empty or too long");
        }
        if self.key_name.is_empty() || self.key_name.len() > 64 {
            return bad("key_name is empty or longer than 64 bytes");
        }
        if self.mount.is_empty() || self.mount.len() > 256 {
            return bad("mount is empty or too long");
        }
        if !(1..=86_400).contains(&self.cache_ttl_seconds) {
            return bad("cache_ttl_seconds must be between 1 and 86400");
        }
        if !(1..=3_600).contains(&self.refresh_interval_seconds) {
            return bad("refresh_interval_seconds must be between 1 and 3600");
        }
        Ok(())
    }

    /// The token and seed files of `site`, checked as paths (not yet as files).
    pub fn for_site(&self, root: &Path, site: &str, sites: usize) -> Result<KmsSite> {
        Ok(KmsSite {
            site: site.to_owned(),
            token_file: site_path("token_file", &self.token_file, root, site, false, sites)?,
            seed_file: site_path("seed_file", &self.seed_file, root, site, true, sites)?,
        })
    }

    /// The transit client configuration for `site`.
    fn transit(&self, site: &KmsSite) -> TransitConfig {
        TransitConfig {
            address: self.address.clone(),
            mount: self.mount.clone(),
            key_name: self.key_name.clone(),
            token_file: site.token_file.clone(),
            namespace: None,
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    fn provider_config(&self, site: &KmsSite) -> ProviderConfig {
        let mut config = ProviderConfig::new(site.seed_file.clone());
        config.cache_ttl = Duration::from_secs(self.cache_ttl_seconds);
        config.refresh_interval = Duration::from_secs(self.refresh_interval_seconds);
        config
    }
}

fn site_path(
    field: &str,
    template: &Path,
    root: &Path,
    site: &str,
    per_site: bool,
    sites: usize,
) -> Result<PathBuf> {
    let no = |why: &str| {
        refuse(
            codes::PATH_REFUSED,
            format!("services.secrets.kms.{field} {why}"),
        )
    };
    let text = template.to_string_lossy();
    if per_site && sites > 1 && !text.contains("{site}") {
        return Err(no("is shared by several sites; put `{site}` in the path"));
    }
    let path = PathBuf::from(text.replace("{site}", site));
    if !path.is_absolute() {
        return Err(no("must be an absolute path"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(no("must not contain `..`"));
    }
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let sites_dir = canon(&root.join("sites"));
    if path
        .parent()
        .map(canon)
        .unwrap_or_default()
        .starts_with(&sites_dir)
    {
        return Err(no(
            "is inside the site folders; a copy of a site would carry it",
        ));
    }
    Ok(path)
}

/// Refuses a file that is missing, not a non-empty regular file, a symbolic link, or accessible
/// to group or others. It reads no content, so nothing secret can reach the message.
fn check_private_file(path: &Path, what: &str) -> Result<()> {
    let at =
        |code: &str, text: &str| refuse(code, format!("the {what} file {} {text}", path.display()));
    let meta = std::fs::symlink_metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => at(codes::FILE_MISSING, "does not exist"),
        _ => at(codes::FILE_UNUSABLE, "cannot be read"),
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
        return Err(at(
            codes::FILE_UNUSABLE,
            "is not a non-empty regular file (a symbolic link is refused)",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(at(
                codes::FILE_UNSAFE,
                &format!("is accessible to group or others (mode {mode:o}); run `chmod 600` on it"),
            ));
        }
    }
    Ok(())
}

/// Asks the shared guard whether the KMS address may be reached, and when not says which policy
/// entry is missing. No connection is made.
async fn check_egress(address: &str, gate: &dyn EgressGate, tenant: &str) -> Result<()> {
    let bad = |why: &str| {
        refuse(
            codes::SETTINGS_INVALID,
            format!("services.secrets.kms.address {why}"),
        )
    };
    let url = Url::parse(address).map_err(|_| bad("is not a URL"))?;
    let host = url.host_str().ok_or_else(|| bad("has no host"))?.to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    let entry = format!(
        "[[tenants.{tenant}.allow]] with scheme = \"https\", host = \"{host}\", ports = [{port}]"
    );
    let text = match gate.authorize(&url).await {
        Ok(_) => return Ok(()),
        Err(EgressError::PolicyDenied) => format!(
            "the egress policy does not allow the KMS; add to the egress policy the entry {entry} (an entry that names a purpose must name `connector`)"
        ),
        Err(EgressError::PortDenied { .. }) => format!(
            "the egress policy refuses port {port} of the KMS; the allow entry for {host} must list it: {entry}"
        ),
        Err(EgressError::AddressDenied { .. }) => format!(
            "{host} resolves to an address the egress policy does not allow; for a KMS on a private network add `allow_private` with its address to the entry {entry}"
        ),
        Err(other) => format!(
            "the egress policy refused the KMS ({}); the entry needed is {entry}",
            other.code()
        ),
    };
    Err(refuse(codes::EGRESS_NOT_ALLOWED, text))
}

/// Counter: KMS key operations refused, by `reason` (closed set [`REASONS`]).
const REFUSED_TOTAL: &str = "spot_secrets_kms_refused_total";

/// The closed set of values of the `reason` label of `spot_secrets_kms_refused_total`.
#[rustfmt::skip]
pub const REASONS: [&str; 9] = [
    "unreachable", "rejected", "egress_refused", "response_invalid", "key_disabled",
    "version_retired", "tampered", "unknown_key", "other",
];

/// The reason of a provider refusal: always one of [`REASONS`].
pub fn reason_of(error: &SecretError) -> &'static str {
    match error {
        SecretError::Tampered => "tampered",
        SecretError::UnknownKey { .. }
        | SecretError::UnknownKeyVersion { .. }
        | SecretError::RetiredKey { .. } => "unknown_key",
        SecretError::Provider(text) => reason_from_text(text),
        _ => "other",
    }
}

/// The reason named by a `secrets_kms.<reason>` code inside `text`, or `other`.
fn reason_from_text(text: &str) -> &'static str {
    REASONS[..6]
        .iter()
        .find(|reason| text.contains(&format!("secrets_kms.{reason}")))
        .copied()
        .unwrap_or("other")
}

/// Reasons that say the KMS itself is not serving (as opposed to a bad value or a retired
/// version): they put the site in the degraded state.
fn is_kms_fault(reason: &str) -> bool {
    matches!(
        reason,
        "unreachable" | "rejected" | "egress_refused" | "response_invalid" | "key_disabled"
    )
}

/// What the KMS connection looks like to readiness. `left` is a lower bound of the time before
/// the oldest possible cached key expires; `reason` is one of [`REASONS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KmsHealth {
    /// The last contact with the KMS worked.
    Ok,
    /// The KMS cannot be reached or refuses, and cached data keys still serve for `left`.
    Degraded {
        /// Time before refusals begin.
        left: Duration,
        /// Why the KMS is not serving.
        reason: &'static str,
    },
    /// The KMS is not serving and no cached key can still serve.
    Refusing {
        /// Why the KMS is not serving.
        reason: &'static str,
    },
}

/// What the last KMS key read said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct SeenKey {
    pub latest_version: u32,
    pub min_decryption_version: u32,
    pub enabled: bool,
}

impl From<KeyInfo> for SeenKey {
    fn from(info: KeyInfo) -> Self {
        Self {
            latest_version: info.latest_version,
            min_decryption_version: info.min_decryption_version,
            enabled: info.enabled,
        }
    }
}

#[derive(Default)]
struct Inner {
    /// Monotonic time of the last contact that proves the KMS answers.
    last_ok: Duration,
    failing: Option<&'static str>,
    last_probe: Option<Duration>,
    cache_epoch: Duration,
    seen: SeenKey,
    refused: BTreeMap<&'static str, u64>,
}

/// The shared state between the provider, the service and readiness. It holds counts, versions
/// and reason codes only.
pub struct KmsState {
    clock: Arc<dyn Clock>,
    cache_ttl: Duration,
    inner: Mutex<Inner>,
}

impl KmsState {
    fn new(clock: Arc<dyn Clock>, cache_ttl: Duration) -> Self {
        let now = clock.monotonic();
        Self {
            clock,
            cache_ttl,
            inner: Mutex::new(Inner {
                last_ok: now,
                cache_epoch: now,
                ..Inner::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Plain counters: a panic elsewhere cannot leave them half-updated.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn contact_ok(&self) {
        let now = self.clock.monotonic();
        let mut inner = self.lock();
        inner.last_ok = now;
        inner.failing = None;
    }

    fn refused(&self, reason: &'static str) {
        metrics::counter!(REFUSED_TOTAL, "reason" => reason).increment(1);
        *self.lock().refused.entry(reason).or_insert(0) += 1;
    }

    fn note(&self, error: &SecretError) {
        let reason = reason_of(error);
        self.refused(reason);
        if is_kms_fault(reason) {
            self.lock().failing = Some(reason);
        }
    }

    /// The health now. The time left is `cache_ttl` minus the time since the last contact that
    /// proved the KMS answers (a probe, or a wrap): a lower bound, because a key unwrapped after
    /// that contact lives a little longer.
    pub fn health(&self) -> KmsHealth {
        let now = self.clock.monotonic();
        let inner = self.lock();
        let Some(reason) = inner.failing else {
            return KmsHealth::Ok;
        };
        let left = self
            .cache_ttl
            .saturating_sub(now.saturating_sub(inner.last_ok));
        if left.is_zero() {
            KmsHealth::Refusing { reason }
        } else {
            KmsHealth::Degraded { left, reason }
        }
    }

    /// The refusals counted since start, by reason.
    pub fn refused_counts(&self) -> BTreeMap<&'static str, u64> {
        self.lock().refused.clone()
    }

    /// What the last key read said.
    pub fn seen_key(&self) -> SeenKey {
        self.lock().seen
    }
}

/// [`KmsKeyProvider`] with refusal counting. Every refusal is counted by reason; a refusal that
/// says the KMS is not serving also sets the degraded state. Nothing else is changed: the answers
/// are the inner provider's.
pub struct KmsHostProvider {
    inner: Arc<KmsKeyProvider>,
    state: Arc<KmsState>,
}

impl KeyProvider for KmsHostProvider {
    fn current(&self) -> KeyRef {
        self.inner.current()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(async move {
            match self.inner.wrap(data_key, aad).await {
                Ok(wrapped) => {
                    // A wrap always reaches the KMS: it proves the KMS answers.
                    self.state.contact_ok();
                    Ok(wrapped)
                }
                Err(e) => {
                    self.state.note(&e);
                    Err(e)
                }
            }
        })
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            // A success may come from the cache, so it proves nothing about the KMS.
            self.inner.unwrap(key, wrapped, aad).await.inspect_err(|e| {
                self.state.note(e);
            })
        })
    }

    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        self.inner.index_key(key)
    }

    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        self.inner.derive_key(key, info, len)
    }
}

/// Checks the token file, asks the guard about the KMS address and opens the transit client.
async fn connect(
    settings: &KmsSettings,
    site: &KmsSite,
    gate: Arc<dyn EgressGate>,
    sender: Arc<dyn Sender>,
) -> Result<Arc<TransitClient>> {
    settings.validate()?;
    check_private_file(&site.token_file, "token")?;
    check_egress(&settings.address, gate.as_ref(), &site.site).await?;
    let client = TransitClient::open(settings.transit(site), sender, gate)
        .await
        .map_err(|e| {
            refuse(
                codes::START_FAILED,
                format!(
                    "the KMS client for key `{}` did not open with the token file {} ({e})",
                    settings.key_name,
                    site.token_file.display()
                ),
            )
        })?;
    Ok(Arc::new(client))
}

fn start_failed(settings: &KmsSettings, e: &SecretError) -> Error {
    refuse(
        codes::START_FAILED,
        format!("the KMS key `{}` is not usable ({e})", settings.key_name),
    )
}

/// Builds the site's KMS key provider and vault, sending through the shared egress `guard`.
pub async fn build_kms_provider(
    settings: &KmsSettings,
    site: &KmsSite,
    guard: Arc<Guard>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
) -> Result<Arc<KmsHost>> {
    let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), site.site.clone(), Purpose::Connector),
        REQUEST_TIMEOUT,
    ));
    build_kms_provider_with(settings, site, guard, sender, clock, entropy).await
}

/// [`build_kms_provider`] with the caller's `sender` (tests send to an in-process fake; the
/// guard still decides whether the address may be reached).
pub async fn build_kms_provider_with(
    settings: &KmsSettings,
    site: &KmsSite,
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
) -> Result<Arc<KmsHost>> {
    check_private_file(&site.seed_file, "seed")?;
    open_host(settings, site, guard, sender, clock, entropy).await
}

/// Creates the seed file (first start of a site on this KMS key): reads the key, makes the seed,
/// wraps it under the KMS key and stores it. It refuses when the file is already there.
pub async fn init_seed(
    settings: &KmsSettings,
    site: &KmsSite,
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
) -> Result<()> {
    if std::fs::symlink_metadata(&site.seed_file).is_ok() {
        return Err(refuse(
            codes::SEED_EXISTS,
            format!(
                "the seed file {} already exists; init never overwrites it (a new seed would change every blind index)",
                site.seed_file.display()
            ),
        ));
    }
    open_host(settings, site, guard, sender, clock, entropy)
        .await
        .map(|_| ())
}

/// The shared part: token file, egress check, transit client, provider (which makes the seed file
/// when it is not there), assembly.
async fn open_host(
    settings: &KmsSettings,
    site: &KmsSite,
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
) -> Result<Arc<KmsHost>> {
    let gate: Arc<dyn EgressGate> = Arc::new(GuardGate::new(guard, site.site.clone()));
    let client = connect(settings, site, gate, sender).await?;
    let kms = KmsKeyProvider::open(
        Arc::clone(&client) as Arc<dyn KmsClient>,
        settings.provider_config(site),
        Arc::clone(&clock),
        entropy,
    )
    .await
    .map_err(|e| start_failed(settings, &e))?;
    let cache_ttl = Duration::from_secs(settings.cache_ttl_seconds);
    let state = Arc::new(KmsState::new(Arc::clone(&clock), cache_ttl));
    let kms = Arc::new(kms);
    let provider = Arc::new(KmsHostProvider {
        inner: Arc::clone(&kms),
        state: Arc::clone(&state),
    });
    Ok(Arc::new(KmsHost {
        key_name: settings.key_name.clone(),
        site: site.clone(),
        refresh_interval: Duration::from_secs(settings.refresh_interval_seconds),
        vault: Arc::new(SecretBox::new(Arc::clone(&provider))),
        provider,
        kms,
        client,
        state,
        clock,
    }))
}

/// The canary the start-up check wraps and unwraps. It is not a secret.
const CANARY: &[u8] = b"spot secrets kms start-up canary";
const CANARY_AAD: &[u8] = b"spot-secrets/kms-canary";

/// Equality that takes the same time wherever the first difference is.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One site's KMS-backed key provider and vault.
pub struct KmsHost {
    key_name: String,
    site: KmsSite,
    refresh_interval: Duration,
    kms: Arc<KmsKeyProvider>,
    client: Arc<TransitClient>,
    provider: Arc<KmsHostProvider>,
    vault: Arc<SecretBox<Arc<KmsHostProvider>>>,
    state: Arc<KmsState>,
    clock: Arc<dyn Clock>,
}

impl KmsHost {
    /// The vault the engine seals with.
    pub fn vault(&self) -> Arc<dyn SecretVault> {
        self.vault.clone()
    }

    /// The key provider the vault uses.
    pub fn provider(&self) -> &Arc<KmsHostProvider> {
        &self.provider
    }

    /// The provider without the refusal counting (rotation and flush use it).
    pub fn kms(&self) -> &Arc<KmsKeyProvider> {
        &self.kms
    }

    /// The shared state: health, refusal counts.
    pub fn state(&self) -> &Arc<KmsState> {
        &self.state
    }

    /// The files of the site.
    pub fn files(&self) -> &KmsSite {
        &self.site
    }

    /// The readiness check: degraded, never a stop.
    pub fn readiness(&self) -> Arc<KmsReadiness> {
        Arc::new(KmsReadiness {
            state: Arc::clone(&self.state),
        })
    }

    /// The required service: start-up check, then the probe and flush loop.
    pub fn service(self: &Arc<Self>) -> Arc<KmsService> {
        Arc::new(KmsService {
            host: Arc::clone(self),
        })
    }

    /// Reads the key, checks it is enabled, wraps and unwraps a canary under the current version
    /// and derives the blind-index key.
    pub async fn startup_check(&self) -> Result<()> {
        let info = self.client.key_info().await.map_err(|e| {
            refuse(
                codes::START_FAILED,
                format!("the KMS key `{}` cannot be read ({e})", self.key_name),
            )
        })?;
        if !info.enabled {
            return Err(refuse(
                codes::START_FAILED,
                format!("the KMS key `{}` is disabled in the KMS", self.key_name),
            ));
        }
        let fail = |why: &str| {
            refuse(
                codes::CANARY_FAILED,
                format!(
                    "the KMS key `{}` failed its start-up check ({why})",
                    self.key_name
                ),
            )
        };
        let current = self.provider.current();
        let wrapped = self
            .provider
            .wrap(CANARY, CANARY_AAD)
            .await
            .map_err(|e| fail(e.code()))?;
        if wrapped.key.id != current.id {
            return Err(fail("the wrapping key is not the current one"));
        }
        let back = self
            .provider
            .unwrap(&wrapped.key, &wrapped, CANARY_AAD)
            .await
            .map_err(|e| fail(e.code()))?;
        if !ct_eq(back.expose(), CANARY) {
            return Err(fail("the canary did not come back"));
        }
        let index = self
            .provider
            .index_key(&self.provider.current())
            .await
            .map_err(|e| fail(e.code()))?;
        if index.expose().is_empty() {
            return Err(fail("the blind-index key is empty"));
        }
        self.state.lock().seen = info.into();
        Ok(())
    }

    /// Moves the derivation seed's wrapping to the latest key version: the last step before the
    /// old version may be retired in the KMS. Returns the version it is now under.
    pub async fn rewrap_seed(&self) -> Result<u32> {
        // The seed is wrapped by an encrypt call, so the provider's view of the latest version
        // must be fresh first.
        let _ = self.kms.refresh().await;
        self.kms.rewrap_seed().await.map_err(|e| {
            self.state.note(&e);
            refuse(
                codes::REWRAP_FAILED,
                format!(
                    "the seed file {} was not moved ({})",
                    self.site.seed_file.display(),
                    e.code()
                ),
            )
        })
    }

    /// One pass of the service loop: a flush request, then a probe when one is due, then the
    /// status file when something changed.
    pub async fn tick(&self) {
        let mut dirty = false;
        if take_flush_request(&self.site.seed_file) {
            self.kms.flush_cache();
            self.state.lock().cache_epoch = self.clock.monotonic();
            tracing::info!(site = %self.site.site, "secrets_kms: key cache flushed on request");
            dirty = true;
        }
        let now = self.clock.monotonic();
        let due = self
            .state
            .lock()
            .last_probe
            .is_none_or(|at| now.saturating_sub(at) >= self.refresh_interval);
        if due {
            self.probe(now).await;
            dirty = true;
        }
        if dirty {
            self.write_status();
        }
    }

    /// Re-reads the token file and the key's versions. A failure is a fault (degraded), never a
    /// stop.
    async fn probe(&self, now: Duration) {
        self.state.lock().last_probe = Some(now);
        // A rotated token file is picked up; on a failed read the old token stays in use.
        let _ = self.client.reload_token().await;
        let _ = self.kms.refresh().await;
        match self.client.key_info().await {
            Ok(info) => {
                self.state.lock().seen = info.into();
                if info.enabled {
                    self.state.contact_ok();
                } else {
                    self.state.lock().failing = Some("key_disabled");
                }
            }
            Err(e) => {
                let reason = reason_from_text(e.code());
                self.state.lock().failing = Some(reason);
                tracing::warn!(
                    site = %self.site.site,
                    code = e.code(),
                    "secrets_kms: the KMS did not answer the key read"
                );
            }
        }
    }

    /// The status the CLI reads.
    pub fn status_snapshot(&self) -> StatusFile {
        let now = self.clock.monotonic();
        let (health, seconds_left) = match self.state.health() {
            KmsHealth::Ok => ("ok", None),
            KmsHealth::Degraded { left, .. } => ("degraded", Some(left.as_secs())),
            KmsHealth::Refusing { .. } => ("refusing", Some(0)),
        };
        let seen = self.state.seen_key();
        let inner = self.state.lock();
        StatusFile {
            updated_unix: self.clock.now_utc().unix_timestamp(),
            key_name: self.key_name.clone(),
            latest_version: seen.latest_version.max(self.kms.current().version),
            min_decryption_version: seen.min_decryption_version,
            key_enabled: seen.enabled,
            cache_entries: self.kms.cache_len(),
            cache_age_seconds: now.saturating_sub(inner.cache_epoch).as_secs(),
            health: health.to_owned(),
            seconds_left,
            last_error: inner.failing.map(|r| format!("secrets_kms.{r}")),
            refused: inner
                .refused
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
        }
    }

    fn write_status(&self) {
        let snapshot = self.status_snapshot();
        if write_status_file(&self.site.seed_file, &snapshot).is_err() {
            tracing::debug!(site = %self.site.site, "secrets_kms: the status file was not written");
        }
    }
}

/// The readiness entry: always ready, with a `degraded:` detail while the KMS is away.
pub struct KmsReadiness {
    state: Arc<KmsState>,
}

impl ReadinessCheck for KmsReadiness {
    fn name(&self) -> &str {
        SERVICE_NAME
    }

    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        let health = self.state.health();
        Box::pin(async move {
            let detail = match health {
                KmsHealth::Ok => String::new(),
                KmsHealth::Degraded { left, reason } => format!(
                    "degraded: the KMS is not serving (secrets_kms.{reason}); cached data keys serve for {} more seconds, then sealed values are refused",
                    left.as_secs()
                ),
                KmsHealth::Refusing { reason } => format!(
                    "degraded: the KMS is not serving (secrets_kms.{reason}) and sealed values are being refused"
                ),
            };
            CheckResult { ok: true, detail }
        })
    }
}

/// The hosted service: required (a start-up failure stops the site), a probe loop after.
pub struct KmsService {
    host: Arc<KmsHost>,
}

impl Service for KmsService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.host.startup_check().await.map_err(|e| e.0) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(TICK) => {}
                }
                self.host.tick().await;
            }
        })
    }
}

/// What the running service writes for `spot secrets-kms status`: counts, versions and reason
/// codes; no key, token, address or value. `health` is `ok`, `degraded` or `refusing`;
/// `seconds_left` is the time before refusals begin while degraded; `last_error` is the last KMS
/// fault's code until a contact works again; `refused` counts refusals since start by reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct StatusFile {
    pub updated_unix: i64,
    pub key_name: String,
    pub latest_version: u32,
    pub min_decryption_version: u32,
    pub key_enabled: bool,
    pub cache_entries: usize,
    pub cache_age_seconds: u64,
    pub health: String,
    pub seconds_left: Option<u64>,
    pub last_error: Option<String>,
    pub refused: BTreeMap<String, u64>,
}

fn beside(seed: &Path, suffix: &str) -> PathBuf {
    let mut name = seed.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Where the running service writes its status.
pub fn status_path(seed_file: &Path) -> PathBuf {
    beside(seed_file, ".status.json")
}

/// The marker `flush-cache` creates and the service removes.
pub fn flush_marker_path(seed_file: &Path) -> PathBuf {
    beside(seed_file, ".flush")
}

fn write_status_file(seed_file: &Path, status: &StatusFile) -> std::io::Result<()> {
    use std::io::Write;
    let temp = beside(seed_file, ".status.tmp");
    let bytes = serde_json::to_vec_pretty(status)
        .map_err(|_| std::io::Error::other("the status cannot be encoded"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, status_path(seed_file))
}

/// Whether a flush request was waiting (it is removed).
fn take_flush_request(seed_file: &Path) -> bool {
    std::fs::remove_file(flush_marker_path(seed_file)).is_ok()
}
