//! The configuration file (TOML), its limits and its validation, and the startup error.
//!
//! Secrets never appear in the file: the store key, the operator secret and (optionally) the
//! database URL are read from files named here. An unknown key is an error, so a typo is refused
//! rather than ignored. See `docs/running-spot/store-server.md`.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use spot_app_review::ReviewConfig;

use crate::error::codes;

/// The most bytes a configuration file may hold.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// The contract's bundle part limit: 64 MiB.
pub const CONTRACT_MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// The contract's manifest limit: 64 KiB.
pub const CONTRACT_MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// A failure to start: a stable code and a sentence that names no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError {
    /// The stable `store_server.*` code.
    pub code: &'static str,
    /// The sentence.
    pub message: String,
}

impl StartupError {
    /// An error with `code` and a message free of secrets.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::new(codes::CONFIG, message)
    }

    pub(crate) fn startup(message: impl Into<String>) -> Self {
        Self::new(codes::STARTUP, message)
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StartupError {}

/// Size and time limits of the HTTP surface.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// The largest `bundle` part, in bytes (at most the contract's 64 MiB).
    pub max_bundle_bytes: u64,
    /// The largest `manifest.toml`, in bytes (at most the contract's 64 KiB).
    pub max_manifest_bytes: u64,
    /// The largest text part (`signature`, one `certificate_chain` entry), in bytes.
    pub max_text_part_bytes: usize,
    /// The largest JSON request body, in bytes.
    pub max_json_bytes: usize,
    /// The longest `Authorization` credential, in bytes.
    pub max_credential_bytes: usize,
    /// How long a whole upload may take, in seconds.
    pub upload_timeout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bundle_bytes: CONTRACT_MAX_BUNDLE_BYTES,
            max_manifest_bytes: CONTRACT_MAX_MANIFEST_BYTES,
            max_text_part_bytes: 16 * 1024,
            max_json_bytes: 8 * 1024,
            max_credential_bytes: 8 * 1024,
            upload_timeout_secs: 300,
        }
    }
}

/// The server's own OS threads for blocking registry, index and state calls.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workers {
    /// Plain OS threads that run synchronous store calls (never `spawn_blocking`).
    pub threads: usize,
    /// Calls that may wait for a thread; a full queue answers `503` with `Retry-After`.
    pub queue: usize,
}

impl Default for Workers {
    fn default() -> Self {
        Self {
            threads: 8,
            queue: 64,
        }
    }
}

/// The PostgreSQL pool and the shared bridge in front of it.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Connections in the pool.
    pub pool_size: usize,
    /// The bridge's `max_in_flight`; must exceed `workers.threads + review.workers + 1`.
    pub max_in_flight: usize,
    /// The bridge's queue capacity beyond `max_in_flight`.
    pub queue_capacity: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            pool_size: 16,
            max_in_flight: 12,
            queue_capacity: 64,
        }
    }
}

/// When the signed revocation list is reissued and how long it lives.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RevocationConfig {
    /// Reissue (with a higher generation, even with unchanged entries) at this interval.
    pub refresh_secs: i64,
    /// A list's validity: `next_update` is `issued_at` plus this.
    pub lifetime_secs: i64,
}

impl Default for RevocationConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 6 * 3600,
            lifetime_secs: 24 * 3600,
        }
    }
}

/// One rate rule: at most `max` calls per `window_secs` per key.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The calls allowed in one window.
    pub max: u32,
    /// The window, in seconds.
    pub window_secs: u32,
}

/// The rate rules of every class (per credential, and per source address).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rates {
    /// `store_public`, per source address.
    pub public_per_address: Rule,
    /// `store_publish`, per publisher key.
    pub publish_per_key: Rule,
    /// `store_publish`, per source address (all keys together).
    pub publish_per_address: Rule,
    /// `store_review`, per publisher key.
    pub review_per_key: Rule,
    /// Operator routes, per source address.
    pub admin_per_address: Rule,
    /// Failed credential checks, per source address.
    pub auth_failures_per_address: Rule,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            public_per_address: Rule {
                max: 600,
                window_secs: 60,
            },
            publish_per_key: Rule {
                max: 10,
                window_secs: 60,
            },
            publish_per_address: Rule {
                max: 30,
                window_secs: 60,
            },
            review_per_key: Rule {
                max: 120,
                window_secs: 60,
            },
            admin_per_address: Rule {
                max: 120,
                window_secs: 60,
            },
            auth_failures_per_address: Rule {
                max: 20,
                window_secs: 300,
            },
        }
    }
}

/// The review workers.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewSection {
    /// Dispatcher threads, which is also the most reviews running (or stuck) at once.
    pub workers: usize,
    /// Reviews that may wait; beyond this a publish is refused with `503`.
    pub queue: usize,
    /// Wall-clock limit of one review, in seconds.
    pub timeout_secs: u64,
    /// The review pipeline's own limits (`spot-app-review`'s configuration).
    pub profile: ReviewConfig,
}

impl Default for ReviewSection {
    fn default() -> Self {
        Self {
            workers: 2,
            queue: 16,
            timeout_secs: 120,
            profile: ReviewConfig::default(),
        }
    }
}

/// A trust root the publisher certificates chain to: its public key lives in a file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRootConfig {
    /// A file holding the standard-base64 of the 32-byte Ed25519 public key.
    pub public_key_file: PathBuf,
    /// First instant (UTC seconds) the root is valid; 0 by default.
    #[serde(default)]
    pub valid_from: i64,
    /// First instant it is no longer valid; absent means never.
    #[serde(default)]
    pub valid_to: Option<i64>,
}

/// The whole configuration.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The listen address, `host:port`.
    pub listen: String,
    /// The directory of the content-addressed bundle store.
    pub blob_dir: PathBuf,
    /// The directory of in-flight uploads (private to the service).
    pub temp_dir: PathBuf,
    /// The store key file: 64 hex digits, mode 0600.
    pub store_key_file: PathBuf,
    /// The operator secret file, mode 0600.
    pub admin_key_file: PathBuf,
    /// A file holding the PostgreSQL URL (use this for a URL that carries a password).
    #[serde(default)]
    pub database_url_file: Option<PathBuf>,
    /// The PostgreSQL URL itself, when it carries no secret. Exactly one of the two is set.
    #[serde(default)]
    pub database_url: Option<String>,
    /// Extra roots publisher certificates may chain to. The store key is always a root.
    #[serde(default)]
    pub trust_roots: Vec<TrustRootConfig>,
    /// App names that are paid: their bundles need an entitled site licence (until WP-STR-08).
    #[serde(default)]
    pub paid_apps: Vec<String>,
    /// Size and time limits.
    #[serde(default)]
    pub limits: Limits,
    /// The synchronous-call threads.
    #[serde(default)]
    pub workers: Workers,
    /// The pool and the bridge.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// The revocation list schedule.
    #[serde(default)]
    pub revocation: RevocationConfig,
    /// Rate rules.
    #[serde(default)]
    pub rates: Rates,
    /// Review workers.
    #[serde(default)]
    pub review: ReviewSection,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The database URL can carry a password: print nothing of it.
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("blob_dir", &self.blob_dir)
            .finish_non_exhaustive()
    }
}

/// A smallest valid configuration for tests and examples (memory-backed deployments).
impl Config {
    /// A configuration with the given paths and defaults everywhere else.
    #[must_use]
    pub fn minimal(blob_dir: PathBuf, temp_dir: PathBuf) -> Self {
        Self {
            listen: "127.0.0.1:0".to_owned(),
            blob_dir,
            temp_dir,
            store_key_file: PathBuf::new(),
            admin_key_file: PathBuf::new(),
            database_url_file: None,
            database_url: None,
            trust_roots: Vec::new(),
            paid_apps: Vec::new(),
            limits: Limits::default(),
            workers: Workers::default(),
            database: DatabaseConfig::default(),
            revocation: RevocationConfig::default(),
            rates: Rates::default(),
            review: ReviewSection::default(),
        }
    }

    /// Reads and validates the file at `path`.
    ///
    /// # Errors
    /// `store_server.config` for an unreadable, unparsable or invalid file.
    pub fn load(path: &Path) -> Result<Self, StartupError> {
        let meta = std::fs::metadata(path)
            .map_err(|_| StartupError::config("the configuration file cannot be read"))?;
        if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
            return Err(StartupError::config(
                "the configuration file is not a regular file under 1 MiB",
            ));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|_| StartupError::config("the configuration file is not text"))?;
        let config: Self = toml::from_str(&text)
            .map_err(|e| StartupError::config(format!("the configuration does not parse: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// The address to listen on.
    ///
    /// # Errors
    /// `store_server.config` when `listen` is not `host:port` with an IP address.
    pub fn listen_addr(&self) -> Result<SocketAddr, StartupError> {
        self.listen
            .parse()
            .map_err(|_| StartupError::config("listen is not an ip:port address"))
    }

    /// Checks every bound the server relies on.
    ///
    /// # Errors
    /// `store_server.config` naming the first setting that does not hold.
    pub fn validate(&self) -> Result<(), StartupError> {
        self.listen_addr()?;
        let bad = |what: &str| Err(StartupError::config(what.to_owned()));
        let limits = &self.limits;
        if limits.max_bundle_bytes == 0 || limits.max_bundle_bytes > CONTRACT_MAX_BUNDLE_BYTES {
            return bad("limits.max_bundle_bytes must be 1 to 67108864 (the contract's 64 MiB)");
        }
        if limits.max_manifest_bytes == 0 || limits.max_manifest_bytes > CONTRACT_MAX_MANIFEST_BYTES
        {
            return bad("limits.max_manifest_bytes must be 1 to 65536 (the contract's 64 KiB)");
        }
        if limits.max_text_part_bytes < 1024 || limits.max_text_part_bytes > 1024 * 1024 {
            return bad("limits.max_text_part_bytes must be 1024 to 1048576");
        }
        if limits.max_json_bytes < 256 || limits.max_json_bytes > 1024 * 1024 {
            return bad("limits.max_json_bytes must be 256 to 1048576");
        }
        if limits.max_credential_bytes < 64 || limits.max_credential_bytes > 64 * 1024 {
            return bad("limits.max_credential_bytes must be 64 to 65536");
        }
        if limits.upload_timeout_secs == 0 {
            return bad("limits.upload_timeout_secs must be above zero");
        }
        if self.workers.threads == 0 || self.workers.queue == 0 {
            return bad("workers.threads and workers.queue must be above zero");
        }
        if self.review.workers == 0 || self.review.queue == 0 || self.review.timeout_secs == 0 {
            return bad("review.workers, review.queue and review.timeout_secs must be above zero");
        }
        let database = &self.database;
        if self.workers.threads + self.review.workers + 1 >= database.max_in_flight {
            return bad(
                "database.max_in_flight must exceed workers.threads plus review.workers plus the maintenance thread, so the bridge never refuses a call as busy",
            );
        }
        if database.max_in_flight + 2 > database.pool_size {
            return bad("database.pool_size must be at least database.max_in_flight plus 2");
        }
        let revocation = &self.revocation;
        let range = 3600..=7 * 24 * 3600;
        if !range.contains(&revocation.refresh_secs) || !range.contains(&revocation.lifetime_secs) {
            return bad("revocation.refresh_secs and lifetime_secs must be 1 hour to 7 days");
        }
        if revocation.refresh_secs >= revocation.lifetime_secs {
            return bad(
                "revocation.refresh_secs must be below lifetime_secs, or the list goes stale before it is reissued",
            );
        }
        for rule in [
            &self.rates.public_per_address,
            &self.rates.publish_per_key,
            &self.rates.publish_per_address,
            &self.rates.review_per_key,
            &self.rates.admin_per_address,
            &self.rates.auth_failures_per_address,
        ] {
            if rule.max == 0 || rule.window_secs == 0 {
                return bad("every rate rule needs max and window_secs above zero");
            }
        }
        for name in &self.paid_apps {
            if spot_registry::AppName::parse(name).is_err() {
                return bad("paid_apps holds a name that is not an app name");
            }
        }
        Ok(())
    }

    /// The database URL: from the file when `database_url_file` is set, else the inline value.
    ///
    /// # Errors
    /// `store_server.config` unless exactly one source is set; `store_server.key_file` for an
    /// unreadable or too permissive file.
    pub fn database_url_text(&self) -> Result<String, StartupError> {
        match (&self.database_url_file, &self.database_url) {
            (Some(path), None) => crate::keys::read_secret_text(path),
            (None, Some(url)) => Ok(url.clone()),
            _ => Err(StartupError::config(
                "set exactly one of database_url_file and database_url",
            )),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config::minimal(PathBuf::from("blobs"), PathBuf::from("tmp"))
    }

    #[test]
    fn the_defaults_are_valid() {
        base().validate().unwrap();
    }

    #[test]
    fn unknown_keys_are_refused() {
        let text = r#"
listen = "127.0.0.1:8480"
blob_dir = "b"
temp_dir = "t"
store_key_file = "k"
admin_key_file = "a"
surprise = 1
"#;
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn a_minimal_file_parses_with_defaults() {
        let text = r#"
listen = "127.0.0.1:8480"
blob_dir = "b"
temp_dir = "t"
store_key_file = "k"
admin_key_file = "a"
database_url_file = "d"
"#;
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.limits.max_bundle_bytes, 67_108_864);
        assert_eq!(config.revocation.lifetime_secs, 86_400);
    }

    #[test]
    fn limits_above_the_contract_are_refused() {
        let mut config = base();
        config.limits.max_bundle_bytes = CONTRACT_MAX_BUNDLE_BYTES + 1;
        assert!(config.validate().is_err());
        let mut config = base();
        config.limits.max_manifest_bytes = CONTRACT_MAX_MANIFEST_BYTES + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_revocation_schedule_is_bounded() {
        let mut config = base();
        config.revocation.refresh_secs = 60;
        assert!(config.validate().is_err());
        let mut config = base();
        config.revocation.lifetime_secs = 8 * 24 * 3600;
        assert!(config.validate().is_err());
        let mut config = base();
        config.revocation.refresh_secs = config.revocation.lifetime_secs;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_threads_must_fit_under_the_bridge() {
        let mut config = base();
        config.workers.threads = 12;
        assert!(config.validate().is_err());
    }

    #[test]
    fn exactly_one_database_source_is_required() {
        let mut config = base();
        assert!(config.database_url_text().is_err());
        config.database_url = Some("postgres://u@h/db".to_owned());
        assert!(config.database_url_text().is_ok());
        config.database_url_file = Some(PathBuf::from("x"));
        assert!(config.database_url_text().is_err());
    }

    #[test]
    fn debug_never_prints_the_database_url() {
        let mut config = base();
        config.database_url = Some("postgres://u:hunter2@h/db".to_owned());
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
