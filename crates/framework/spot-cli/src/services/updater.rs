//! The updater (WP-PRD-03.T5): what `spot update` and the optional `updater` service share.
//!
//! `spot-updater` verifies and applies; this module talks to a server and to the host. A source
//! (an HTTPS server, or an offline folder, `spot update --repo DIR`) has one layout:
//! `<base>/<channel>/update.toml`, `update.sig`, `revocations.toml` and the archive the manifest
//! names. Whatever the configuration or the flags:
//!
//! - **TLS only.** The URL is `https`, the certificate is verified against the platform roots, a
//!   redirect is an error (an update can neither be downgraded to `http` nor sent elsewhere).
//! - **The trust anchor is local:** `update-trust.toml` under the install root, installed with the
//!   package. Nothing the server sends can add a key.
//! - **Signature before bytes.** Manifest, signature and revocation list are verified before the
//!   archive is requested; the archive is fetched with a size cap from the signed manifest and its
//!   digest is checked before anything is staged.
//! - **Time is an argument** (the service takes it from the site clock it is given).
//! - **Telemetry is opt-in:** off, the request has no query; on, only `v`, `p` and `c`.
//!
//! `[services.updater]`: `mode` is `off`, `notify` (default) or `auto` (apply inside `window` when
//! the host is idle). It looks at most once a day; `auto` starts a separate `spot update apply`
//! process, because the update restarts the service that would run it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_updater::canonical::unb64;
use spot_updater::{
    ApplyRequest, ArchiveLimits, Channel, DataHooks, Decision, Digest, FileSystem, Installed,
    KeyId, Layout, Outcome, PublicKey, RevokedUpdateKey, SIGNATURE_BYTES, ServiceControl,
    TrustedUpdateKey, UpdateEngine, UpdateManifest, UpdatePolicy, UpdateRevocation,
    UpdateRevocationBody, UpdateSignature, UpdateTrust, UpdaterError, verify_update,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use url::Url;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: late, after every service that stores or serves anything.
pub const ORDER: u32 = 1090;
/// The trust anchor, under the install root.
pub const TRUST_FILE: &str = "update-trust.toml";
/// The revocation generation already seen.
pub const GENERATION_FILE: &str = "update-revocation-generation";
/// What the last check found.
pub const STATUS_FILE: &str = "update-status.toml";
/// What the installed version records about itself.
pub const STATE_FILE: &str = "update-state.toml";
/// The verified files of a downloaded update.
pub const CACHE_DIR: &str = "update-cache";
/// While this file exists the host is not idle and `auto` waits.
pub const HOLD_FILE: &str = "update.hold";
/// The shortest time between two looks at the server.
pub const MIN_CHECK_INTERVAL_SECS: i64 = 86_400;
/// After a failed look, how soon the service tries again.
pub const RETRY_AFTER_FAILURE_SECS: i64 = 3_600;
/// The largest manifest, signature or revocation list that is read.
pub const SMALL_FILE_LIMIT: u64 = 1 << 20;

fn bad(key: &str, why: &str) -> Error {
    Error(format!("services.updater.{key}: {why}"))
}

fn io(detail: impl Into<String>) -> UpdaterError {
    UpdaterError::Io {
        detail: detail.into(),
    }
}

type Fail<T> = std::result::Result<T, UpdaterError>;

// --- settings ----------------------------------------------------------------------------------

/// `[services.updater]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `off`, `notify` (the default) or `auto`.
    pub mode: String,
    /// The maintenance window, `HH:MM-HH:MM` UTC (may cross midnight). Absent: any time.
    pub window: Option<String>,
    /// `stable` (the default) or `beta`.
    pub channel: String,
    /// The `https` base URL of the update source. Absent: nothing is checked.
    pub url: Option<String>,
    /// The install root (versions, pointers, journal). Absent: the host root.
    pub install_root: Option<PathBuf>,
    /// The product a manifest must carry.
    pub product: String,
    /// The schema epoch of the installed database, until the installed version records its own.
    pub schema_epoch: u64,
    /// Opt in to sending installed version, platform and channel with the check.
    pub telemetry: bool,
    /// The service manager's name for the host (`systemctl <verb> <unit>`).
    pub unit: String,
    /// The `host:port` polled for readiness after a restart; absent: `server.bind`.
    pub ready_addr: Option<String>,
    /// How long the new version has to become ready, in seconds.
    pub health_timeout_secs: i64,
    /// How long after a commit a failing new version still rolls back, in seconds.
    pub probation_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "notify".to_owned(),
            window: None,
            channel: "stable".to_owned(),
            url: None,
            install_root: None,
            product: "spotledger".to_owned(),
            schema_epoch: 0,
            telemetry: false,
            unit: "spot".to_owned(),
            ready_addr: None,
            health_timeout_secs: 60,
            probation_secs: 300,
        }
    }
}

/// How the service behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Does nothing.
    Off,
    /// Looks and records; changes nothing.
    Notify,
    /// Applies inside the window when the host is idle.
    Auto,
}

/// The settings that needed parsing.
#[derive(Clone, Debug)]
pub struct Parsed {
    /// The mode.
    pub mode: Mode,
    /// The maintenance window.
    pub window: Option<Window>,
    /// The channel.
    pub channel: Channel,
    /// The `https` source.
    pub url: Option<Url>,
}

impl Settings {
    /// Checks every setting; a problem names the key, never a value that could be a secret.
    ///
    /// # Errors
    /// A bad mode, window, channel or timeout, or a URL that is not `https` or carries a user
    /// name or password.
    pub fn validate(&self) -> Result<Parsed> {
        let mode = match self.mode.as_str() {
            "off" => Mode::Off,
            "notify" => Mode::Notify,
            "auto" => Mode::Auto,
            _ => return Err(bad("mode", "must be off, notify or auto")),
        };
        if self.health_timeout_secs <= 0 || self.probation_secs == 0 {
            return Err(bad("health_timeout_secs", "timeouts must be positive"));
        }
        if self.product.is_empty() || self.unit.is_empty() {
            return Err(bad("product", "product and unit must not be empty"));
        }
        Ok(Parsed {
            mode,
            window: self.window.as_deref().map(Window::parse).transpose()?,
            channel: Channel::parse(&self.channel)
                .map_err(|_| bad("channel", "must be stable or beta"))?,
            url: self.url.as_deref().map(parse_https).transpose()?,
        })
    }

    /// The policy for one channel: exactly that channel is allowed.
    #[must_use]
    pub fn policy(&self, channel: Channel) -> UpdatePolicy {
        UpdatePolicy {
            allowed_channels: vec![channel],
            probation_secs: self.probation_secs,
            health_timeout_secs: self.health_timeout_secs,
            ..UpdatePolicy::default()
        }
    }
}

/// Parses an update URL: `https` only, no credentials, a host.
///
/// # Errors
/// Anything else; the message never echoes the URL (it may carry a token).
pub fn parse_https(text: &str) -> Result<Url> {
    let url = Url::parse(text).map_err(|_| bad("url", "is not a URL"))?;
    if url.scheme() != "https" {
        return Err(bad(
            "url",
            "must be https (an update is never fetched in the clear)",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return Err(bad("url", "must have a host and no user name or password"));
    }
    Ok(url)
}

/// A daily maintenance window in UTC, to the minute; it may cross midnight (`22:00-02:00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    start: u32,
    end: u32,
}

impl Window {
    /// Parses `HH:MM-HH:MM`.
    ///
    /// # Errors
    /// Not two valid times, or equal ends (that is no window).
    pub fn parse(text: &str) -> Result<Self> {
        let minutes = |part: &str| -> Option<u32> {
            let (h, m) = part.split_once(':')?;
            let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
            (h < 24 && m < 60 && part.len() == 5).then_some(h * 60 + m)
        };
        match text
            .split_once('-')
            .and_then(|(a, b)| Some((minutes(a)?, minutes(b)?)))
        {
            Some((start, end)) if start != end => Ok(Self { start, end }),
            _ => Err(bad(
                "window",
                "must be HH:MM-HH:MM (UTC) with different ends",
            )),
        }
    }

    /// Whether `now` (Unix seconds) is inside the window.
    #[must_use]
    pub fn contains(&self, now: i64) -> bool {
        let minute = u32::try_from(now.rem_euclid(86_400) / 60).unwrap_or(0);
        if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}

// --- the install root's files ------------------------------------------------------------------

fn read_text(fs: &dyn FileSystem, path: &Path) -> Fail<String> {
    String::from_utf8(fs.read(path)?).map_err(|_| io("a file is not UTF-8 text"))
}

fn read_toml<T: serde::de::DeserializeOwned>(fs: &dyn FileSystem, path: &Path) -> Option<T> {
    toml::from_str(&read_text(fs, path).ok()?).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    format: String,
    #[serde(default)]
    keys: Vec<TrustKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustKey {
    public: String,
    #[serde(default)]
    valid_from: i64,
    valid_to: Option<i64>,
}

/// Reads the trust anchor installed with the package (`format = "spot-update-trust"`, then
/// `[[keys]]` of base64 `public`, `valid_from`, optional `valid_to`).
///
/// # Errors
/// A missing, malformed or empty anchor: with none, nothing can be trusted and nothing runs.
pub fn load_trust(fs: &dyn FileSystem, root: &Path) -> Fail<UpdateTrust> {
    let malformed = |detail: &str| UpdaterError::KeyMalformed {
        detail: format!("{TRUST_FILE}: {detail}"),
    };
    let file: TrustFile = toml::from_str(&read_text(fs, &root.join(TRUST_FILE))?)
        .map_err(|e| malformed(e.message()))?;
    if file.format != "spot-update-trust" || file.keys.is_empty() {
        return Err(malformed(
            "not a `spot-update-trust` file with at least one key",
        ));
    }
    let mut keys = Vec::new();
    for key in file.keys {
        let bytes = unb64(&key.public).ok_or_else(|| malformed("a key is not base64"))?;
        let public = PublicKey::from_bytes(&bytes)?;
        keys.push(TrustedUpdateKey::new(public, key.valid_from, key.valid_to));
    }
    Ok(UpdateTrust::new(keys))
}

/// The revocation generation already seen, 0 when none.
pub fn held_generation(fs: &dyn FileSystem, root: &Path) -> u64 {
    read_text(fs, &root.join(GENERATION_FILE))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// What the installed version records about itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The schema epoch of the database.
    pub schema_epoch: Option<u64>,
    /// The installed payload's SHA-256 (hex).
    pub payload_sha256: Option<String>,
}

/// What the last check found.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// When the check ran (Unix seconds).
    pub checked_at: i64,
    /// The channel looked at.
    pub channel: String,
    /// `current`, `available`, `refused` or `failed`.
    pub state: String,
    /// The version on offer, when there is one.
    pub available: Option<String>,
    /// One plain sentence, never a secret.
    pub detail: String,
}

/// Reads the last status.
pub fn read_status(fs: &dyn FileSystem, root: &Path) -> Option<Status> {
    read_toml(fs, &root.join(STATUS_FILE))
}

/// Records the status.
///
/// # Errors
/// A failed write.
pub fn write_status(fs: &dyn FileSystem, root: &Path, status: &Status) -> Fail<()> {
    let text = toml::to_string(status).map_err(|_| io("the status did not encode"))?;
    fs.write_atomic(&root.join(STATUS_FILE), text.as_bytes())
}

/// Reads a pointer file; `None` when absent.
///
/// # Errors
/// A pointer that does not name a version.
pub fn read_pointer(fs: &dyn FileSystem, path: &Path) -> Fail<Option<Version>> {
    if !fs.exists(path)? {
        return Ok(None);
    }
    Version::parse(read_text(fs, path)?.trim())
        .map(Some)
        .map_err(|_| UpdaterError::Pointer {
            detail: "a version pointer does not name a version".to_owned(),
        })
}

/// The installed version as the decision sees it.
///
/// # Errors
/// [`UpdaterError::Pointer`] when `current` is missing: the updater works on a versioned install.
pub fn installed(
    fs: &dyn FileSystem,
    layout: &Layout,
    settings: &Settings,
    channel: Channel,
) -> Fail<Installed> {
    let version =
        read_pointer(fs, &layout.current_path())?.ok_or_else(|| UpdaterError::Pointer {
            detail: "there is no `current` pointer: this is not a versioned install".to_owned(),
        })?;
    let state: State = read_toml(fs, &layout.root().join(STATE_FILE)).unwrap_or_default();
    Ok(Installed {
        product: settings.product.clone(),
        version,
        channel,
        schema_epoch: state.schema_epoch.unwrap_or(settings.schema_epoch),
        payload_sha256: state
            .payload_sha256
            .and_then(|hex| Digest::from_hex(&hex).ok()),
    })
}

// --- the HTTPS client ----------------------------------------------------------------------------

/// A minimal HTTPS GET client: TLS 1.2+ on `ring`, certificate and name verified, no redirects, a
/// deadline per request and a cap on the body.
#[derive(Clone)]
pub struct HttpsClient {
    config: Arc<rustls::ClientConfig>,
    timeout: Duration,
}

impl HttpsClient {
    /// A client that trusts the platform's root certificates.
    ///
    /// # Errors
    /// No usable root certificate: nothing could then be verified.
    pub fn platform() -> Result<Self> {
        let loaded = rustls_native_certs::load_native_certs();
        Self::with_roots(loaded.certs.iter().map(|c| c.as_ref().to_vec()).collect())
    }

    /// A client that trusts exactly the given DER certificates (a private authority, or a test's).
    ///
    /// # Errors
    /// No certificate was usable.
    pub fn with_roots(roots: Vec<Vec<u8>>) -> Result<Self> {
        let mut store = rustls::RootCertStore::empty();
        for der in roots {
            let _ = store.add(rustls::pki_types::CertificateDer::from(der));
        }
        if store.is_empty() {
            return Err(Error(
                "no usable root certificate to verify the update server".into(),
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| Error("TLS versions are not available".into()))?
            .with_root_certificates(store)
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
            timeout: Duration::from_secs(60),
        })
    }

    /// Fetches `url` (https only) and returns the body, at most `limit` bytes.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] for a connection, TLS or protocol failure, a status other than 200 (a
    /// redirect included), a body over `limit`, or a body whose length is not stated. The message
    /// never contains the URL.
    pub async fn get(&self, url: &Url, limit: u64) -> Fail<Vec<u8>> {
        if url.scheme() != "https" {
            return Err(io("an update is only fetched over https"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| io("the update URL has no host"))?;
        let bare = host.trim_matches(['[', ']']).to_owned();
        let name = rustls::pki_types::ServerName::try_from(bare.clone())
            .map_err(|_| io("the update server name is not valid"))?;
        let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        // HTTP/1.0: the server answers with a content length and no chunking.
        let request = format!(
            "GET {}{query} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: spot-update\r\nConnection: close\r\n\r\n",
            url.path()
        );
        let port = url.port().unwrap_or(443);
        let work = async {
            let tcp = tokio::net::TcpStream::connect((bare.as_str(), port))
                .await
                .map_err(|_| io("the update server did not accept the connection"))?;
            let mut tls = tokio_rustls::TlsConnector::from(self.config.clone())
                .connect(name, tcp)
                .await
                .map_err(|_| io("the TLS handshake with the update server failed"))?;
            tls.write_all(request.as_bytes())
                .await
                .map_err(|_| io("the request could not be sent"))?;
            let (mut raw, cap) = (Vec::new(), limit.saturating_add(65_536));
            // A missing close_notify ends the read in an error; the content length proves the body.
            let _ = (&mut tls).take(cap + 1).read_to_end(&mut raw).await;
            if raw.len() as u64 > cap {
                return Err(io("the response is larger than the size limit"));
            }
            parse_response(&raw, limit)
        };
        tokio::time::timeout(self.timeout, work)
            .await
            .map_err(|_| io("the update server did not answer in time"))?
    }
}

fn parse_response(raw: &[u8], limit: u64) -> Fail<Vec<u8>> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io("the response has no header end"))?;
    let head =
        std::str::from_utf8(&raw[..split]).map_err(|_| io("the response header is not text"))?;
    let mut lines = head.split("\r\n");
    let code = lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .nth(1)
        .unwrap_or_default();
    if code != "200" {
        return Err(if code.starts_with('3') {
            io("the update server answered with a redirect; redirects are not followed")
        } else {
            io(format!("the update server answered {code}"))
        });
    }
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or_else(|| io("the response does not state its length"))?;
    let body = &raw[split + 4..];
    if body.len() < length {
        return Err(io("the response is shorter than it declared"));
    }
    if length as u64 > limit {
        return Err(io("the response is larger than the size limit"));
    }
    Ok(body[..length].to_vec())
}

// --- sources and offers --------------------------------------------------------------------------

/// Where an update comes from.
#[derive(Clone)]
pub enum Source {
    /// An `https` server; `query` is the opt-in telemetry query.
    Https {
        /// The base URL.
        base: Url,
        /// The client.
        client: HttpsClient,
        /// The query, when telemetry is on.
        query: Option<String>,
    },
    /// An offline repository folder (or the cache) with the same layout.
    Dir(PathBuf),
}

impl Source {
    /// Reads `<channel>/<name>`, at most `limit` bytes.
    ///
    /// # Errors
    /// [`UpdaterError::Io`], or [`UpdaterError::ArchiveEscape`] for a name that is not one plain
    /// file name.
    pub async fn read(&self, channel: Channel, name: &str, limit: u64) -> Fail<Vec<u8>> {
        if name.is_empty() || name == ".." || name == "." || name.contains(['/', '\\', ':', '\0']) {
            return Err(UpdaterError::ArchiveEscape {
                entry: name.to_owned(),
            });
        }
        match self {
            Source::Dir(dir) => {
                let file = std::fs::File::open(dir.join(channel.as_str()).join(name))
                    .map_err(|_| io(format!("{name} is not in the repository")))?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::Read::take(file, limit + 1), &mut bytes)
                    .map_err(|_| io(format!("{name} could not be read")))?;
                if bytes.len() as u64 > limit {
                    return Err(io(format!("{name} is larger than the size limit")));
                }
                Ok(bytes)
            }
            Source::Https {
                base,
                client,
                query,
            } => {
                let mut url = base.clone();
                url.path_segments_mut()
                    .map_err(|_| io("the update URL cannot be a base"))?
                    .pop_if_empty()
                    .push(channel.as_str())
                    .push(name);
                url.set_query(query.as_deref());
                client.get(&url, limit).await
            }
        }
    }
}

/// What a source offers: the three signed documents, parsed, with their original text kept so a
/// download is stored exactly as it was verified.
pub struct Offer {
    /// The manifest.
    pub manifest: UpdateManifest,
    /// The detached signature.
    pub signature: UpdateSignature,
    /// The revocation list.
    pub revocations: UpdateRevocation,
    /// `update.toml`, `update.sig` and `revocations.toml` as received.
    pub texts: [String; 3],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevocationsFile {
    format: String,
    generation: u64,
    issued_at: i64,
    issued_by: String,
    signature: String,
    #[serde(default)]
    keys: Vec<RevokedEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedEntry {
    key_id: String,
    reason: String,
}

/// Parses `revocations.toml` (`format = "spot-update-revocations"`, `generation`, `issued_at`,
/// `issued_by`, base64 `signature`, `[[keys]]` of `key_id` and `reason`). The signature is checked
/// later, by `verify_update`.
///
/// # Errors
/// [`UpdaterError::SignatureMalformed`] for any malformed field.
pub fn parse_revocations(text: &str) -> Fail<UpdateRevocation> {
    let malformed = |detail: &str| UpdaterError::SignatureMalformed {
        detail: format!("revocations: {detail}"),
    };
    let file: RevocationsFile = toml::from_str(text).map_err(|e| malformed(e.message()))?;
    if file.format != "spot-update-revocations" {
        return Err(malformed("wrong format"));
    }
    let signature = unb64(&file.signature)
        .and_then(|bytes| <[u8; SIGNATURE_BYTES]>::try_from(bytes.as_slice()).ok())
        .ok_or_else(|| malformed("the signature is not 64 bytes of base64"))?;
    let mut keys = Vec::new();
    for entry in file.keys {
        keys.push(RevokedUpdateKey {
            key_id: KeyId::parse(&entry.key_id)?,
            reason: entry.reason,
        });
    }
    // The issuer signs the list sorted by key id; sort the same way so the bytes agree.
    keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    let body = UpdateRevocationBody {
        generation: file.generation,
        issued_at: file.issued_at,
        issued_by: KeyId::parse(&file.issued_by)?,
        keys,
    };
    Ok(UpdateRevocation::from_parts(body, signature))
}

/// Fetches the manifest, its signature and the revocation list. Nothing is verified here.
///
/// # Errors
/// A document that cannot be read or does not parse.
pub async fn fetch_offer(source: &Source, channel: Channel) -> Fail<Offer> {
    let mut texts = Vec::new();
    for name in ["update.toml", "update.sig", "revocations.toml"] {
        let bytes = source.read(channel, name, SMALL_FILE_LIMIT).await?;
        texts.push(String::from_utf8(bytes).map_err(|_| io("an update document is not text"))?);
    }
    let [manifest, signature, revocations]: [String; 3] = texts
        .try_into()
        .map_err(|_| io("an update document is missing"))?;
    Ok(Offer {
        manifest: UpdateManifest::from_toml(&manifest)?,
        signature: UpdateSignature::from_toml(&signature)?,
        revocations: parse_revocations(&revocations)?,
        texts: [manifest, signature, revocations],
    })
}

/// Looks at a source: fetch, verify (signature, trust, key window, revocation), decide, and
/// remember the revocation generation. The order is the point: nothing is decided or shown from
/// content that did not verify.
///
/// # Errors
/// A fetch, parse or verification failure, with its stable code.
pub async fn check(
    source: &Source,
    fs: &dyn FileSystem,
    root: &Path,
    installed: &Installed,
    policy: &UpdatePolicy,
    now: i64,
) -> Fail<(Offer, Decision)> {
    let trust = load_trust(fs, root)?;
    let offer = fetch_offer(source, installed.channel).await?;
    let held = held_generation(fs, root);
    let generation = offer.revocations.body.generation;
    let signature = Some(&offer.signature);
    verify_update(
        &offer.manifest,
        signature,
        &trust,
        &offer.revocations,
        now,
        held,
    )?;
    if generation > held {
        fs.durable_write(
            &root.join(GENERATION_FILE),
            generation.to_string().as_bytes(),
        )?;
    }
    let decision = spot_updater::decide(installed, &offer.manifest, policy, now);
    Ok((offer, decision))
}

/// The seams an update runs over.
pub struct Seams<'a> {
    /// The file system.
    pub fs: &'a dyn FileSystem,
    /// The service control.
    pub service: &'a dyn ServiceControl,
    /// The data hooks.
    pub hooks: &'a dyn DataHooks,
}

/// Applies an offer through the update engine, which verifies again from the bytes it is given,
/// and records what the installed version now is.
pub fn apply(
    seams: &Seams<'_>,
    layout: &Layout,
    (trust, installed, offer): (&UpdateTrust, Installed, &Offer),
    (payload, policy, now): (&[u8], UpdatePolicy, i64),
) -> Outcome {
    let root = layout.root();
    let engine = UpdateEngine::new(seams.fs, seams.service, seams.hooks, layout.clone());
    let request = ApplyRequest {
        installed,
        manifest: offer.manifest.clone(),
        signature: Some(offer.signature.clone()),
        trust,
        revocations: &offer.revocations,
        held_revocation_generation: held_generation(seams.fs, root),
        payload,
        policy,
        limits: ArchiveLimits::default(),
    };
    let outcome = engine.apply(&request, now);
    if matches!(outcome, Outcome::Applied { .. }) {
        let state = State {
            schema_epoch: Some(offer.manifest.schema_epoch),
            payload_sha256: Some(offer.manifest.payload.sha256.to_hex()),
        };
        if let Ok(text) = toml::to_string(&state) {
            let _ = seams
                .fs
                .write_atomic(&root.join(STATE_FILE), text.as_bytes());
        }
    }
    outcome
}

// --- the service -------------------------------------------------------------------------------

/// Starts a separate apply process (the update restarts the host, so the host cannot run it).
pub trait Launcher: Send + Sync {
    /// Starts `spot update apply --apply` for `channel` and returns without waiting.
    ///
    /// # Errors
    /// Why it could not be started.
    fn launch(&self, host_root: &Path, channel: Channel) -> std::result::Result<(), String>;
}

/// Says whether the host is idle (no job holds a lock, no migration runs): `true` lets an update
/// begin. The registered one is "no `update.hold` file under the install root".
pub type Gate = Arc<dyn Fn() -> bool + Send + Sync>;

/// Launches this executable's own `update apply`, under `systemd-run` when the host runs under
/// systemd, so stopping the unit does not kill the apply.
pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    fn launch(&self, host_root: &Path, channel: Channel) -> std::result::Result<(), String> {
        let exe =
            std::env::current_exe().map_err(|_| "the executable path is unknown".to_owned())?;
        let args = ["update", "apply", "--apply", "--channel", channel.as_str()];
        let mut command = if cfg!(unix) && std::env::var_os("INVOCATION_ID").is_some() {
            let mut c = std::process::Command::new("systemd-run");
            c.args(["--collect", "--quiet", "--unit", "spot-update"])
                .arg(exe);
            c
        } else {
            std::process::Command::new(exe)
        };
        command
            .arg("--root")
            .arg(host_root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|_| "the update process could not be started".to_owned())
    }
}

/// What one look of the service came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cycle {
    /// The mode is `off`.
    Off,
    /// The last look was too recent.
    TooSoon,
    /// Nothing new.
    Current,
    /// An update is available and was recorded.
    Available(Version),
    /// `auto`: the apply process was started.
    Launched(Version),
    /// `auto`: waiting, with the reason.
    Deferred(Version, String),
    /// Refused or failed (the stable code); recorded.
    Failed(String),
}

/// The `updater` service.
pub struct Updater {
    /// The settings.
    pub settings: Settings,
    /// The parsed settings.
    pub parsed: Parsed,
    /// The host root, for the apply process.
    pub host_root: PathBuf,
    /// The install root layout.
    pub layout: Layout,
    /// Where updates come from.
    pub source: Source,
    /// The file system.
    pub fs: Arc<dyn FileSystem>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// Starts the apply process.
    pub launcher: Arc<dyn Launcher>,
    /// Says whether the host is idle.
    pub gate: Gate,
}

impl Updater {
    /// One look: at most daily; records the result; in `auto` starts the apply when allowed.
    pub async fn cycle(&self) -> Cycle {
        let (fs, root, channel) = (self.fs.as_ref(), self.layout.root(), self.parsed.channel);
        if self.parsed.mode == Mode::Off {
            return Cycle::Off;
        }
        let now = self.clock.now_utc().unix_timestamp();
        if let Some(last) = read_status(fs, root) {
            let wait = if last.state == "failed" {
                RETRY_AFTER_FAILURE_SECS
            } else {
                MIN_CHECK_INTERVAL_SECS
            };
            if now.saturating_sub(last.checked_at) < wait {
                return Cycle::TooSoon;
            }
        }
        let record = |state: &str, available: Option<&Version>, detail: String| {
            let status = Status {
                checked_at: now,
                channel: channel.as_str().to_owned(),
                state: state.to_owned(),
                available: available.map(ToString::to_string),
                detail,
            };
            let _ = write_status(fs, root, &status);
        };
        let failed = |error: UpdaterError| {
            record("failed", None, error.to_string());
            Cycle::Failed(error.code().as_str().to_owned())
        };
        let installed = match installed(fs, &self.layout, &self.settings, channel) {
            Ok(installed) => installed,
            Err(error) => return failed(error),
        };
        let policy = self.settings.policy(channel);
        match check(&self.source, fs, root, &installed, &policy, now).await {
            Err(error) => failed(error),
            Ok((_, Decision::Skip(reason))) => {
                record("current", None, reason);
                Cycle::Current
            }
            Ok((_, Decision::Refuse(code))) => {
                record("refused", None, code.as_str().to_owned());
                Cycle::Failed(code.as_str().to_owned())
            }
            Ok((offer, Decision::StepVia(via))) => {
                let version = offer.manifest.version;
                record("available", Some(&version), format!("install {via} first"));
                Cycle::Available(version)
            }
            Ok((offer, Decision::Apply)) => {
                let version = offer.manifest.version;
                record(
                    "available",
                    Some(&version),
                    "an update is available".to_owned(),
                );
                if self.parsed.mode != Mode::Auto {
                    return Cycle::Available(version);
                }
                if self.parsed.window.is_some_and(|w| !w.contains(now)) {
                    return Cycle::Deferred(version, "outside the maintenance window".to_owned());
                }
                if !(self.gate)() {
                    return Cycle::Deferred(version, "the host is busy".to_owned());
                }
                match self.launcher.launch(&self.host_root, channel) {
                    Ok(()) => Cycle::Launched(version),
                    Err(reason) => Cycle::Deferred(version, reason),
                }
            }
        }
    }
}

impl Service for Updater {
    fn name(&self) -> &'static str {
        "updater"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            // First look after 5 minutes plus a stable per-root offset, then hourly wake-ups.
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in self.layout.root().to_string_lossy().bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
            let mut wait = Duration::from_secs(300 + hash % 5_100);
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(wait) => {}
                }
                let report = self.cycle().await;
                tracing::info!(report = ?report, "updater cycle");
                wait = Duration::from_secs(3_600);
            }
        })
    }
}

/// Registers the `updater` service: nothing when the mode is `off` or no `url` is set, and only on
/// the first site (the updater belongs to the host, not to a site).
///
/// # Errors
/// A setting that does not validate (a plain `http` URL included) stops the host rather than
/// running with a policy nobody meant.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("updater")?;
    let parsed = settings.validate()?;
    let Some(url) = parsed.url.clone().filter(|_| parsed.mode != Mode::Off) else {
        return Ok(());
    };
    if !ctx.assembling() {
        return Ok(());
    }
    let Ok(env) = ctx.env() else {
        return Ok(());
    };
    if env.loaded.sites.first().map(|s| s.site.as_str()) != Some(env.site.site.as_str()) {
        return Ok(());
    }
    let host_root = env.root.to_path_buf();
    let install = settings
        .install_root
        .clone()
        .unwrap_or_else(|| host_root.clone());
    let source = Source::Https {
        base: url,
        client: HttpsClient::platform()?,
        query: None,
    };
    let updater = Updater {
        gate: {
            let hold = install.join(HOLD_FILE);
            Arc::new(move || !hold.exists())
        },
        layout: Layout::new(install),
        settings,
        parsed,
        host_root,
        source,
        fs: Arc::new(spot_updater::StdFileSystem::new()),
        clock: ctx.clock().clone(),
        launcher: Arc::new(ProcessLauncher),
    };
    ctx.add(Arc::new(updater));
    Ok(())
}
