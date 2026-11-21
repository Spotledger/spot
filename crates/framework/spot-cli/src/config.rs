//! Strict host and site configuration. Validation finishes before any database side effect.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_http::ServerConfig;
use spot_postgres::PgConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Common host configuration.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Common {
    /// App directories relative to the configuration root (or absolute).
    pub apps: Vec<PathBuf>,
    /// HTTP configuration, checked against the public ServerConfig vocabulary.
    pub server: toml::Value,
    /// Administrative URL and per-site connection defaults.
    pub postgres: Postgres,
    /// Telemetry configuration.
    #[serde(default)]
    pub telemetry: Telemetry,
    /// The framework services `spot serve` hosts. Defaults are the single-node desktop
    /// profile; a cluster sets `profile = "cluster"` and points the same options at shared
    /// infrastructure.
    #[serde(default)]
    pub services: Services,
    /// The identity audit trail's options.
    #[serde(default)]
    pub audit: Audit,
    /// Which publishers this site trusts for `spot install-app`.
    #[serde(default)]
    pub trust: Trust,
    /// Backup targets, retention and verification (`spot backup`).
    #[serde(default)]
    pub backup: Backup,
}
/// The `[trust]` section of `common.toml`: who may sign the apps this site installs.
///
/// Empty means nothing is trusted: every signed bundle is refused until an operator pins a root
/// key. The paths are relative to the configuration root (or absolute).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Trust {
    /// The trust-roots file: one root public key per line (base64, `#` comments allowed), the
    /// format `spot-pack verify --trust` reads.
    pub roots: Option<PathBuf>,
    /// The revocation list the root issued (JSON, as `spot-pack verify --revocations` reads it).
    pub revocations: Option<PathBuf>,
    /// Whether `install-app` refuses what it cannot verify: a plain app directory, and a bundle
    /// without a signature. The default is `true`; a development site sets it to `false`.
    pub require_signed: bool,
}
impl Default for Trust {
    fn default() -> Self {
        Self {
            roots: None,
            revocations: None,
            require_signed: true,
        }
    }
}
/// The `[backup]` table of `common.toml`: where `spot backup` writes and how long it keeps.
///
/// Absent means no targets. The checks here are lexical only (see [`Backup::problems`]); the
/// filesystem checks belong to the run that uses a target.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Backup {
    /// Named targets, `[backup.targets.<name>]`.
    pub targets: BTreeMap<String, BackupTarget>,
    /// How many backups each target keeps.
    pub retention: BackupRetention,
    /// `quick` or `full`: how a written backup is checked.
    pub verify: String,
    /// The target used when a command names none.
    pub default_target: Option<String>,
    /// The directory of the PostgreSQL client tools (`pg_dump`). The first source of the order
    /// `[backup] pg_bin_dir`, then `SPOT_PG_BIN`, then `PATH`. Absolute, without `..`.
    pub pg_bin_dir: Option<PathBuf>,
}
impl Default for Backup {
    fn default() -> Self {
        Self {
            targets: BTreeMap::new(),
            retention: BackupRetention::default(),
            verify: "quick".into(),
            default_target: None,
            pg_bin_dir: None,
        }
    }
}
/// One backup destination.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackupTarget {
    /// A directory on this machine (a mounted USB drive included).
    Folder {
        /// Absolute path of the directory.
        path: PathBuf,
    },
    /// A removable drive (a USB stick or disk) named by label or UUID, never by drive letter or
    /// path. The packages go to `subdir` (default `spot-backups`) on the mounted volume; a drive
    /// that is not plugged in is `backup.target_absent` and nothing is written.
    Removable {
        /// The volume label or UUID (`uuid:<id>` or `label:<name>`).
        volume: String,
        /// The folder on the volume, relative, letters, digits, `-`, `_`, `.`.
        #[serde(default)]
        subdir: Option<String>,
    },
    /// An object store. Parsed and validated here; WP-PRD-04.T5 runs it, through a connector the
    /// host supplies (none is configured on this host yet).
    Cloud {
        /// The store's `https://` endpoint, without credentials.
        endpoint: String,
        /// An identifier of the access key, never key material (the secret comes through the
        /// secrets path).
        key_id: String,
        /// The secret reference (`secret:<name>`) of the access key's secret, never the secret.
        #[serde(default)]
        credential_ref: Option<String>,
        /// A folder-like prefix inside the bucket.
        #[serde(default)]
        prefix: Option<String>,
        /// Upload pacing, kilobits per second (at least 1).
        #[serde(default)]
        max_kbps: Option<u32>,
        /// The most bytes the target may hold.
        #[serde(default)]
        quota_bytes: Option<u64>,
    },
}
/// The retention numbers (the grandfather-father-son window of `spot_backup::RetentionPolicy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupRetention {
    /// Daily backups to keep.
    pub keep_daily: u32,
    /// Weekly backups to keep.
    pub keep_weekly: u32,
    /// Monthly backups to keep.
    pub keep_monthly: u32,
    /// The newest backups always kept, at least 1.
    pub keep_last: u32,
}
impl Default for BackupRetention {
    fn default() -> Self {
        // The library's `RetentionPolicy::default()` values (7, 4, 12); the library has no
        // `keep_last` (it always keeps the newest verified backup), so 1 is the floor.
        Self {
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 12,
            keep_last: 1,
        }
    }
}
/// Why [`Backup::target`] found no target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupConfigError {
    /// `[backup]` configures no target.
    NoTargets,
    /// The named target (or `default_target`) is not configured.
    Unknown(String),
    /// Several targets exist, none was named and there is no `default_target`.
    Ambiguous,
}
impl std::fmt::Display for BackupConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTargets => f.write_str("backup: no targets are configured"),
            Self::Unknown(name) => write!(f, "backup: unknown target {name:?}"),
            Self::Ambiguous => {
                f.write_str("backup: several targets exist; name one or set backup.default_target")
            }
        }
    }
}
impl std::error::Error for BackupConfigError {}
const BACKUP_MAX_TARGETS: usize = 32;
const BACKUP_MAX_COUNT: u32 = 10_000;
impl Backup {
    /// The named target, else `default_target`, else the only target.
    pub fn target(
        &self,
        name: Option<&str>,
    ) -> std::result::Result<(&str, &BackupTarget), BackupConfigError> {
        if self.targets.is_empty() {
            return Err(BackupConfigError::NoTargets);
        }
        let wanted = name.or(self.default_target.as_deref());
        if let Some(wanted) = wanted {
            return self
                .targets
                .get_key_value(wanted)
                .map(|(key, target)| (key.as_str(), target))
                .ok_or_else(|| BackupConfigError::Unknown(wanted.to_owned()));
        }
        let mut all = self.targets.iter();
        match (all.next(), all.next()) {
            (Some((key, target)), None) => Ok((key.as_str(), target)),
            _ => Err(BackupConfigError::Ambiguous),
        }
    }
    /// Every lexical problem, each naming the table path and the rule and never a value
    /// (an endpoint may carry a secret).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.targets.len() > BACKUP_MAX_TARGETS {
            problems.push(format!(
                "backup.targets: at most {BACKUP_MAX_TARGETS} targets"
            ));
        }
        if self.verify != "quick" && self.verify != "full" {
            problems.push("backup.verify: must be quick or full".into());
        }
        let r = &self.retention;
        for (key, count) in [
            ("keep_daily", r.keep_daily),
            ("keep_weekly", r.keep_weekly),
            ("keep_monthly", r.keep_monthly),
            ("keep_last", r.keep_last),
        ] {
            if count > BACKUP_MAX_COUNT {
                problems.push(format!(
                    "backup.retention.{key}: must be at most {BACKUP_MAX_COUNT}"
                ));
            }
        }
        if r.keep_last == 0 {
            problems.push("backup.retention.keep_last: must be at least 1".into());
        }
        if let Some(default) = &self.default_target
            && !self.targets.contains_key(default)
        {
            problems.push("backup.default_target: names no configured target".into());
        }
        if let Some(dir) = &self.pg_bin_dir {
            if dir.as_os_str().is_empty() {
                problems.push("backup.pg_bin_dir: must not be empty".into());
            } else if dir.as_os_str().as_encoded_bytes().contains(&0) {
                problems.push("backup.pg_bin_dir: must not contain a NUL".into());
            } else if dir
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                problems.push("backup.pg_bin_dir: must not contain a '..' component".into());
            } else if !dir.is_absolute() {
                problems.push("backup.pg_bin_dir: must be an absolute path".into());
            }
        }
        let mut paths: BTreeMap<PathBuf, &str> = BTreeMap::new();
        for (name, target) in &self.targets {
            let valid_name = name.len() <= 32
                && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && name.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                });
            if !valid_name {
                problems.push(format!(
                    "backup.targets.{name:?}: name must match [a-z][a-z0-9_-]{{0,31}}"
                ));
                continue;
            }
            let at = format!("backup.targets.{name}");
            match target {
                BackupTarget::Folder { path } => {
                    if path.as_os_str().is_empty() {
                        problems.push(format!("{at}.path: must not be empty"));
                    } else if path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                    {
                        problems.push(format!("{at}.path: must not contain a '..' component"));
                    } else if !path.is_absolute() {
                        problems.push(format!("{at}.path: must be an absolute path"));
                    } else {
                        let normal: PathBuf = path
                            .components()
                            .filter(|c| !matches!(c, std::path::Component::CurDir))
                            .collect();
                        if let Some(first) = paths.insert(normal, name) {
                            problems.push(format!(
                                "{at}.path: names the same path as backup.targets.{first}"
                            ));
                        }
                    }
                }
                BackupTarget::Cloud {
                    endpoint,
                    key_id,
                    credential_ref,
                    prefix,
                    max_kbps,
                    quota_bytes,
                } => {
                    problems.extend(endpoint_problem(endpoint, &at));
                    if key_id.is_empty() || key_id.len() > 128 {
                        problems.push(format!("{at}.key_id: must be 1 to 128 bytes"));
                    }
                    // The rest of the rules live with the adapter's own spec. A configuration
                    // without a credential reference is still valid here (older files); using it
                    // for a run is what needs one.
                    let spec = crate::services::backup_targets::TargetSpec::Cloud {
                        endpoint: endpoint.clone(),
                        key_id: key_id.clone(),
                        credential_ref: credential_ref
                            .clone()
                            .unwrap_or_else(|| "secret:unset".to_owned()),
                        prefix: prefix.clone(),
                        max_kbps: *max_kbps,
                        quota_bytes: *quota_bytes,
                    };
                    for problem in spec.problems() {
                        if ["credential_ref", "prefix", "max_kbps"]
                            .iter()
                            .any(|key| problem.starts_with(key))
                        {
                            problems.push(format!("{at}.{problem}"));
                        }
                    }
                }
                BackupTarget::Removable { volume, subdir } => {
                    let spec = crate::services::backup_targets::TargetSpec::Removable {
                        volume: volume.clone(),
                        subdir: subdir.clone(),
                    };
                    for problem in spec.problems() {
                        problems.push(format!("{at}.{problem}"));
                    }
                }
            }
        }
        problems
    }
}
/// The rule an endpoint breaks, without echoing it.
fn endpoint_problem(endpoint: &str, at: &str) -> Option<String> {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return Some(format!("{at}.endpoint: must be an https:// URL"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty()
        || endpoint
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        Some(format!(
            "{at}.endpoint: must be an https:// URL with a host"
        ))
    } else if authority.contains('@') {
        Some(format!("{at}.endpoint: must not contain userinfo"))
    } else if endpoint.contains('?') || endpoint.contains('#') {
        Some(format!(
            "{at}.endpoint: must not contain a query or fragment"
        ))
    } else {
        None
    }
}
/// The identity audit trail.
///
/// A site with a PostgreSQL database writes its identity events to the hash-chained audit store;
/// these options only say what is written *beside* the chain and how loudly a broken chain is
/// reported. The defaults keep the file log off and readiness warning-only.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audit {
    /// Also append every identity record to `sites/<site>/identity-audit.log` (mode 0600, size
    /// capped), after the chain append. The file is a convenience for log shippers and is never
    /// relied on: its failure does not fail an append.
    pub identity_file_mirror: bool,
    /// Make `/api/health/ready` fail (not only warn) while the identity chain does not verify.
    pub identity_chain_strict: bool,
    /// The document audit's chain lanes (WP-PRD-01.T22): how many independent chains the lane
    /// appenders write (1 to 64). **Fixed for the life of the site**: changing it needs a new
    /// lane epoch (`doc_lane_epoch`) and a migration of the lane heads, which this release does not
    /// provide, so the host refuses to start when the stored checkpoint disagrees.
    pub doc_lanes: u32,
    /// The lane epoch: bumped only together with a lane count change.
    pub doc_lane_epoch: u32,
    /// How often, in milliseconds, one chain-append job per lane is requested (100 to 60000).
    pub doc_append_poll_ms: u64,
    /// How many outbox events one append job drains at most (1 to 5000).
    pub doc_append_batch: u32,
}

impl Default for Audit {
    fn default() -> Self {
        Self {
            identity_file_mirror: false,
            identity_chain_strict: false,
            doc_lanes: 4,
            doc_lane_epoch: 1,
            doc_append_poll_ms: 1_000,
            doc_append_batch: 200,
        }
    }
}
/// Per-service runtime settings.
///
/// Every default is the single-node desktop profile: one process, the in-process metadata
/// bus, an in-process coordinator, the local file provider and mail captured in memory.
/// [`Services::validate`] checks the options against each other and returns **every**
/// problem at once, so one start-up reports the whole configuration.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Services {
    /// `single-node` or `cluster`. A cluster needs a PostgreSQL database for the metadata
    /// version store and the coordinator; a single node keeps both in process.
    pub profile: String,
    /// How long each service may drain while `spot serve` stops (milliseconds).
    pub shutdown_grace_ms: u64,
    /// Metadata: which invalidation bus and how often a node polls the version store.
    pub metadata: Metadata,
    /// Realtime: the feed pump's cadence and the socket heartbeat.
    pub realtime: Realtime,
    /// Jobs: the queues the worker claims, the outbox intake cadence.
    pub jobs: Jobs,
    /// Scheduler: how often the leader plans due slots.
    pub scheduler: SchedulerService,
    /// Outgoing mail.
    pub mail: Mail,
    /// Server-side PDF printing.
    pub print: Print,
    /// File storage.
    pub files: Files,
    /// Prometheus metrics.
    pub metrics: Metrics,
    /// Every `[services.<name>]` table the typed options above do not know: the settings of
    /// the modules under `src/services/*.rs` that register themselves, handed over raw
    /// (`SiteCtx::settings`). [`Services::validate`] refuses a table no registered module
    /// claims, so a misspelt typed table still fails closed.
    #[serde(flatten)]
    pub services_extra: BTreeMap<String, toml::Value>,
}
impl Default for Services {
    fn default() -> Self {
        Self {
            profile: "single-node".into(),
            shutdown_grace_ms: 5_000,
            metadata: Metadata::default(),
            realtime: Realtime::default(),
            jobs: Jobs::default(),
            scheduler: SchedulerService::default(),
            mail: Mail::default(),
            print: Print::default(),
            files: Files::default(),
            metrics: Metrics::default(),
            services_extra: BTreeMap::new(),
        }
    }
}
/// Metadata service options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metadata {
    /// `polling` (the database version store, every node polls it) or `in-process` (one
    /// process, an in-memory broadcast).
    pub bus: String,
    /// Poll interval in milliseconds; at most once per interval per node.
    pub poll_ms: u64,
}
impl Default for Metadata {
    fn default() -> Self {
        Self {
            bus: "in-process".into(),
            poll_ms: 2_000,
        }
    }
}
/// Realtime service options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Realtime {
    /// Whether `/api/v1/realtime` and the feed pump run.
    pub enabled: bool,
    /// Seconds between server WebSocket pings (the contract's default is 25).
    pub heartbeat_seconds: u64,
    /// Feed-pump interval in milliseconds: how often the change feed and the outbox are
    /// read and mapped onto room events.
    pub poll_ms: u64,
    /// The most WebSocket connections one site holds open (default 5000, at most 100000).
    /// Past it an upgrade is refused with 429 `realtime.too_many_connections`.
    pub max_connections: usize,
}
impl Default for Realtime {
    fn default() -> Self {
        Self {
            enabled: true,
            heartbeat_seconds: 25,
            poll_ms: 100,
            max_connections: 5000,
        }
    }
}
/// Job worker and outbox intake options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Jobs {
    /// The queues the worker claims, in order.
    pub queues: Vec<String>,
    /// Outbox intake interval in milliseconds.
    pub intake_poll_ms: u64,
    /// How long a stopping worker waits for its running job (milliseconds).
    pub grace_ms: u64,
    /// `postgres` keeps jobs in the site's database (they survive a crash and a restart);
    /// `memory` keeps them in this process only and is allowed in the single-node profile.
    pub store: String,
    /// The time to live of the queue-consumer lease (milliseconds). The holder renews it at
    /// a third of this; a node that stops renewing is fenced out after it.
    pub lease_ttl_ms: u64,
}
impl Default for Jobs {
    fn default() -> Self {
        Self {
            // The queues a schedule may name (`default`, `short`, `long`): a worker that claims
            // only `default` would leave every `short` and `long` scheduled job pending.
            queues: vec!["default".into(), "short".into(), "long".into()],
            intake_poll_ms: 250,
            grace_ms: 10_000,
            store: "postgres".into(),
            lease_ttl_ms: 15_000,
        }
    }
}
/// Scheduler options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerService {
    /// Tick interval in milliseconds.
    pub tick_ms: u64,
    /// The schedule-leader lease's time to live, in milliseconds.
    pub lease_ttl_ms: u64,
}
impl Default for SchedulerService {
    fn default() -> Self {
        Self {
            tick_ms: 1_000,
            lease_ttl_ms: 30_000,
        }
    }
}
/// Outgoing mail options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mail {
    /// Whether the outgoing queue is sent. A disabled mail service is optional: the site
    /// still serves and reports the degradation on `/api/health/ready`.
    pub enabled: bool,
    /// `smtp` sends through [`Mail::smtp`]; `memory` captures messages in process (tests
    /// and local development).
    pub sink: String,
    /// Where the log of accepted messages lives: `auto` (the default: the site's database,
    /// `spot_mail_sent_log`, when the job store is PostgreSQL, so a restart cannot send an
    /// accepted message again; this process's memory otherwise), `postgres` (the same, and the
    /// site is refused unless the job store is PostgreSQL) or `memory` (this process only: a
    /// restart can send an accepted message again).
    pub sent_log: String,
    /// Where the bounces of mail that could not be delivered are kept: `memory` (the default:
    /// this process only, a restart forgets them) or `postgres` (the site's database,
    /// `spot_mail_bounce_log`, unique on job, recipient and class, so a restart or a second node
    /// neither loses a bounce nor writes it twice; needs `services.jobs.store = "postgres"`).
    pub bounce_log: String,
    /// The SMTP account, required when `sink = "smtp"`. The password is a secret and is
    /// never included in an error, a log line or the readiness report.
    pub smtp: Option<Smtp>,
}
impl Mail {
    /// The sent log in use for a site whose job store is `job_store`: `"postgres"` or
    /// `"memory"`. `auto` follows the job store; an explicit value says so itself.
    #[must_use]
    pub fn effective_sent_log(&self, job_store: &str) -> &'static str {
        match self.sent_log.as_str() {
            "postgres" => "postgres",
            "memory" => "memory",
            _ if job_store == "postgres" => "postgres",
            _ => "memory",
        }
    }

    /// Whether the sent log lives in PostgreSQL for a site whose job store is `job_store`.
    #[must_use]
    pub fn sent_log_in_postgres(&self, job_store: &str) -> bool {
        self.effective_sent_log(job_store) == "postgres"
    }

    /// Whether the bounce log lives in PostgreSQL: only when the operator chose `postgres`.
    #[must_use]
    pub fn bounce_log_in_postgres(&self, _job_store: &str) -> bool {
        self.bounce_log == "postgres"
    }

    /// One line saying which sent log is in use and why, for the start-up log.
    #[must_use]
    pub fn sent_log_description(&self, job_store: &str) -> String {
        let effective = self.effective_sent_log(job_store);
        match (self.sent_log.as_str(), effective) {
            ("auto", "postgres") => "mail sent log: postgres (auto)".into(),
            ("auto", _) => "mail sent log: memory (auto: job store is memory)".into(),
            (_, e) => format!("mail sent log: {e} (explicit)"),
        }
    }
}

impl Default for Mail {
    fn default() -> Self {
        Self {
            enabled: true,
            sink: "memory".into(),
            sent_log: "auto".into(),
            bounce_log: "memory".into(),
            smtp: None,
        }
    }
}
/// One SMTP account.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    /// Server host name.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// `plain`, `starttls` or `tls`.
    pub security: String,
    /// The envelope sender.
    pub from: String,
    /// Optional user name.
    pub username: Option<String>,
    /// Optional password (a secret).
    pub password: Option<String>,
}
/// Server-side print options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Print {
    /// Whether the print endpoint renders PDFs. Optional: a disabled print service is
    /// reported as a degradation.
    pub enabled: bool,
    /// Template root relative to the site directory.
    pub template_root: String,
}
impl Default for Print {
    fn default() -> Self {
        Self {
            enabled: true,
            template_root: "templates".into(),
        }
    }
}
/// File storage options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Files {
    /// `local` (bytes under [`Files::root`]), `memory` (tests), `s3` or `azure` (an object store,
    /// behind the `files-s3` and `files-azure` features).
    pub provider: String,
    /// The local provider's root, relative to the site directory.
    pub root: String,
    /// Hard per-upload limit in bytes.
    pub max_bytes: u64,
    /// The S3-compatible provider's settings (`provider = "s3"` only).
    pub s3: Option<FilesS3>,
    /// The Azure Blob provider's settings (`provider = "azure"` only).
    pub azure: Option<FilesAzure>,
}
impl Default for Files {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            root: "files".into(),
            max_bytes: 25 * 1024 * 1024,
            s3: None,
            azure: None,
        }
    }
}
/// `[services.files.s3]`: an S3-compatible bucket (AWS S3, Cloudflare R2, MinIO, Wasabi).
///
/// Credentials are references (`env:NAME` or `file:/absolute/path`), never values. With neither
/// reference the AWS default chain of the process environment applies.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesS3 {
    /// The bucket; it must exist (the service never creates one).
    pub bucket: String,
    /// The service base URL; absent selects AWS.
    pub endpoint: Option<String>,
    /// The region, for example `us-east-1`.
    pub region: Option<String>,
    /// Bucket-in-path addressing.
    pub path_style: bool,
    /// Plain HTTP (a loopback emulator).
    pub allow_http: bool,
    /// Allows `allow_http` on a remote endpoint. Never for production.
    pub allow_insecure_remote: bool,
    /// Where the access key id lives.
    pub access_key_id_ref: Option<String>,
    /// Where the secret access key lives.
    pub secret_access_key_ref: Option<String>,
}
/// `[services.files.azure]`: an Azure Blob container.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesAzure {
    /// The storage account.
    pub account: String,
    /// The container; it must exist.
    pub container: String,
    /// The service base URL; absent selects Azure.
    pub endpoint: Option<String>,
    /// `access_key` or `sas`.
    pub credential_kind: String,
    /// Where the credential lives: `env:NAME` or `file:/absolute/path`.
    pub credential_ref: Option<String>,
    /// Plain HTTP (the Azurite emulator on loopback).
    pub allow_http: bool,
    /// Allows `allow_http` on a remote endpoint. Never for production.
    pub allow_insecure_remote: bool,
}
/// Metrics options.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    /// Whether `GET /metrics` is served.
    pub enabled: bool,
    /// Whether `/metrics` requires the same bearer credential as `/api/v1`.
    pub require_auth: bool,
}
impl Default for Metrics {
    fn default() -> Self {
        Self {
            enabled: true,
            require_auth: true,
        }
    }
}
impl Services {
    /// Every problem with the service options, judged together.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if !matches!(self.profile.as_str(), "single-node" | "cluster") {
            errors.push("services.profile must be single-node or cluster".into());
        }
        // A table the typed options do not know belongs to a module that registers itself.
        // One that no module claims is a typo or a removed service: refuse it, by name and
        // never by value.
        let known = crate::services::registered_names();
        for (name, table) in &self.services_extra {
            if !table.is_table() {
                errors.push(format!("services.{name}: expected a table"));
            } else if !known.contains(&name.as_str()) {
                let modules = if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                };
                errors.push(format!(
                    "services.{name}: unknown table (modules that read settings: {modules})"
                ));
            }
        }
        if !matches!(self.metadata.bus.as_str(), "in-process" | "polling") {
            errors.push("services.metadata.bus must be in-process or polling".into());
        }
        // A cluster's reasons for existing is cross-node delivery: an in-process bus there
        // would silently split the nodes' metadata.
        if self.profile == "cluster" && self.metadata.bus != "polling" {
            errors.push("services.metadata.bus must be polling in the cluster profile".into());
        }
        if self.metadata.poll_ms == 0 {
            errors.push("services.metadata.poll_ms must be positive".into());
        }
        if self.shutdown_grace_ms == 0 {
            errors.push("services.shutdown_grace_ms must be positive".into());
        }
        if self.realtime.enabled && self.realtime.poll_ms == 0 {
            errors.push("services.realtime.poll_ms must be positive".into());
        }
        if self.realtime.enabled
            && !(1..=spot_realtime::ws::limits::MAX_CONNECTIONS_CEILING)
                .contains(&self.realtime.max_connections)
        {
            errors.push("services.realtime.max_connections must be between 1 and 100000".into());
        }
        if self.realtime.enabled && self.realtime.heartbeat_seconds == 0 {
            errors.push("services.realtime.heartbeat_seconds must be positive".into());
        }
        if self.jobs.queues.is_empty() {
            errors.push("services.jobs.queues must not be empty".into());
        }
        for queue in &self.jobs.queues {
            // A queue name is what a job row carries; keep it to the same shape as a job
            // name so a typo is a start-up error, not a queue nothing claims.
            let valid = !queue.is_empty()
                && queue.len() <= 64
                && queue.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b':'
                });
            if !valid {
                errors.push("services.jobs.queues: invalid queue name".into());
            }
        }
        if !matches!(self.mail.sent_log.as_str(), "auto" | "memory" | "postgres") {
            errors.push("services.mail.sent_log must be auto, memory or postgres".into());
        }
        if !matches!(self.mail.bounce_log.as_str(), "memory" | "postgres") {
            errors.push("services.mail.bounce_log must be memory or postgres".into());
        }
        if self.mail.bounce_log == "postgres" && self.jobs.store != "postgres" {
            errors.push(
                "services.mail.bounce_log = postgres needs services.jobs.store = postgres".into(),
            );
        }
        if self.mail.sent_log == "postgres" && self.jobs.store != "postgres" {
            errors.push(
                "services.mail.sent_log = postgres needs services.jobs.store = postgres".into(),
            );
        }
        if !matches!(self.jobs.store.as_str(), "postgres" | "memory") {
            errors.push("services.jobs.store must be postgres or memory".into());
        }
        if self.profile == "cluster" && self.jobs.store != "postgres" {
            errors.push("services.jobs.store must be postgres in the cluster profile".into());
        }
        if self.jobs.lease_ttl_ms < 300 {
            errors.push("services.jobs.lease_ttl_ms must be at least 300".into());
        }
        if self.jobs.intake_poll_ms == 0 {
            errors.push("services.jobs.intake_poll_ms must be positive".into());
        }
        if self.scheduler.tick_ms == 0 {
            errors.push("services.scheduler.tick_ms must be positive".into());
        }
        if self.scheduler.lease_ttl_ms <= self.scheduler.tick_ms {
            errors.push("services.scheduler.lease_ttl_ms must be longer than tick_ms".into());
        }
        if !matches!(self.mail.sink.as_str(), "smtp" | "memory") {
            errors.push("services.mail.sink must be smtp or memory".into());
        }
        if self.mail.sink == "smtp" && self.mail.smtp.is_none() {
            errors.push("services.mail.smtp is required when sink = smtp".into());
        }
        if let Some(smtp) = &self.mail.smtp {
            if smtp.host.is_empty() || smtp.port == 0 {
                errors.push("services.mail.smtp needs a host and a port".into());
            }
            if !matches!(smtp.security.as_str(), "plain" | "starttls" | "tls") {
                errors.push("services.mail.smtp.security must be plain, starttls or tls".into());
            }
            // The address is validated without echoing it: a bad envelope sender may contain
            // a credential-looking local part.
            let address_ok = smtp.from.len() <= 320
                && !smtp.from.bytes().any(|b| b.is_ascii_whitespace())
                && smtp
                    .from
                    .split_once('@')
                    .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
            if !address_ok {
                errors.push("services.mail.smtp.from is not a valid address".into());
            }
        }
        if !matches!(
            self.files.provider.as_str(),
            "local" | "memory" | "s3" | "azure"
        ) {
            errors.push("services.files.provider must be local, memory, s3 or azure".into());
        }
        errors.extend(crate::services::files_object::validate(&self.files));
        if self.files.provider == "local" && self.files.root.is_empty() {
            errors.push("services.files.root must not be empty".into());
        }
        if self.files.max_bytes == 0 {
            errors.push("services.files.max_bytes must be positive".into());
        }
        for (name, path) in [
            (
                ["services", "print", "template_root"],
                &self.print.template_root,
            ),
            (["services", "files", "root"], &self.files.root),
        ] {
            // A path under the site folder only: no parent step, no absolute path, no drive.
            let leaves = std::path::Path::new(path).components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            });
            if leaves || path.contains('\\') {
                errors.push(format!(
                    "{} must stay inside the site folder",
                    name.join(".")
                ));
            }
        }
        errors
    }
}
/// PostgreSQL connection defaults. The URL is never included in error output.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Postgres {
    /// URL for database creation.
    pub admin_url: String,
    /// Maximum per-site pooled connections.
    #[serde(default = "pool_size")]
    pub pool_max_size: usize,
    /// How long a request waits for a pooled connection before it is answered as unavailable, in
    /// milliseconds (100 to 60,000). Absent: the driver's default of five seconds.
    #[serde(default)]
    pub pool_wait_ms: Option<u64>,
}

impl Postgres {
    /// `pg` with this section's pool settings applied.
    #[must_use]
    pub fn apply_pool(&self, mut pg: spot_postgres::PgConfig) -> spot_postgres::PgConfig {
        pg.pool_max_size = self.pool_max_size;
        if let Some(ms) = self.pool_wait_ms {
            pg.pool_wait_timeout = std::time::Duration::from_millis(ms);
        }
        pg
    }
}
fn pool_size() -> usize {
    16
}
/// Serializable telemetry settings mapped onto spot-telemetry.
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    /// Optional HTTP/protobuf trace exporter.
    pub otlp: Option<Otlp>,
    /// Tracing filter (default info).
    pub filter: Option<String>,
    /// json or pretty.
    pub format: Option<String>,
    /// Exported service name.
    pub service_name: Option<String>,
    /// Exported node identity.
    pub node_id: Option<String>,
}
/// OTLP settings; sampling uses integer parts per million.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Otlp {
    /// HTTP collector endpoint.
    pub endpoint: String,
    /// Share of traces, from zero through one million.
    pub sample_ppm: u32,
}
impl Telemetry {
    /// Produces the telemetry crate's configuration.
    pub fn config(&self) -> spot_telemetry::TelemetryConfig {
        let mut c = spot_telemetry::TelemetryConfig::default();
        if let Some(v) = &self.filter {
            c.filter.clone_from(v);
        }
        if self.format.as_deref() == Some("pretty") {
            c.format = spot_telemetry::LogFormat::Pretty;
        }
        if let Some(v) = &self.service_name {
            c.service_name.clone_from(v);
        }
        if let Some(v) = &self.node_id {
            c.node_id.clone_from(v);
        }
        c.otlp = self.otlp.as_ref().map(|o| spot_telemetry::OtlpConfig {
            endpoint: o.endpoint.clone(),
            sample_ppm: o.sample_ppm,
        });
        c
    }
}
/// One site. Only key digests are persisted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// Site identity.
    pub site: String,
    /// PostgreSQL database name.
    pub database: String,
    /// ICU locale.
    #[serde(default = "default_collation")]
    pub collation: String,
    /// Additional HTTP host names.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// API key verifiers.
    #[serde(default)]
    pub api_keys: Vec<Key>,
    /// Installed apps, in install order. Empty means a host-wide site: every directory in
    /// `common.toml` `apps` is hosted and no platform app is installed. When not empty the
    /// first entry is always `platform` and the others are manifest names found in the
    /// `common.toml` app catalogue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// Password verifiers set with `spot set-password` (Argon2id, never the password).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
    /// The partition profile of this site (WP-PRD-01.T11). Absent means a single-node site: it
    /// installs no owner check and reports `partition: off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<Partition>,
}
/// The partition profile of one site: what makes a node of a multi-node deployment.
///
/// A node is partitioned when it names itself and lists the DocTypes that are partitioned. Every
/// listed DocType must carry scope fields, or the site refuses to start with
/// `partition.scope_uncovered`: a partitioned DocType nobody configured is unprotected. The scope
/// field list is configuration until ADR-063 is accepted (WP-EDG-03.T7 open question 1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Partition {
    /// This node's identity in the deployment.
    pub node: String,
    /// The partitioned DocTypes. Empty means nothing is partitioned, which a node profile may not
    /// be: it would claim the profile and check no write.
    pub doctypes: Vec<String>,
    /// Which field of which DocType supplies a scope key.
    pub scope: Vec<ScopeField>,
}
/// One scope field: `field` of `doctype` supplies a scope key of type `key`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeField {
    /// The DocType.
    pub doctype: String,
    /// The scope key type (never interpreted by the platform, ADR-062).
    pub key: String,
    /// The field that carries the key.
    pub field: String,
}
impl Partition {
    /// Why this profile cannot be served, one message per problem; empty when it can.
    ///
    /// Only the shape is checked here: whether every partitioned DocType is covered is decided
    /// against the site's served metadata, by `OwnerParticipant::checked` at start-up.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.node.is_empty()
            || self
                .node
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        {
            problems.push("partition.node must be a node id of letters, digits, '-' or '_'".into());
        }
        if self.doctypes.is_empty() {
            problems.push("partition.doctypes must name at least one partitioned DocType".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for doctype in &self.doctypes {
            if doctype.is_empty() {
                problems.push("partition.doctypes has an empty entry".into());
            } else if !seen.insert(doctype.as_str()) {
                problems.push(format!("partition.doctypes names `{doctype}` twice"));
            }
        }
        let mut pairs = std::collections::BTreeSet::new();
        for entry in &self.scope {
            if entry.doctype.is_empty() || entry.key.is_empty() || entry.field.is_empty() {
                problems.push(
                    "partition.scope has an entry with an empty doctype, key or field".into(),
                );
            } else if !pairs.insert((entry.doctype.as_str(), entry.key.as_str())) {
                problems.push(format!(
                    "partition.scope names `{}`'s `{}` key twice",
                    entry.doctype, entry.key
                ));
            }
        }
        problems
    }
}
/// A stored password verifier for a user of the platform app.
///
/// The platform's `user.password_hash` field has no storage column until the secrets service
/// lands (WP-SEC-10), so the verifier lives next to the API key verifiers, in the protected
/// site file.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The user's immutable record id.
    pub user: String,
    /// Argon2id PHC string.
    pub password_hash: String,
    /// The verifier belongs to a one-time first sign-in secret: signing in with it requires
    /// choosing a password, after which the secret no longer works.
    #[serde(default, skip_serializing_if = "is_false")]
    pub first_sign_in: bool,
    /// Unix seconds at which a one-time secret stops working. A `first_sign_in` verifier
    /// without it (written by an earlier version) is treated as expired, never as unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}
impl Site {
    /// A site with no keys or credentials yet.
    pub fn named(site: &str, database: &str, collation: &str, apps: Vec<String>) -> Self {
        Self {
            site: site.into(),
            database: database.into(),
            collation: collation.into(),
            aliases: Vec::new(),
            api_keys: Vec::new(),
            apps,
            credentials: Vec::new(),
            partition: None,
        }
    }
}
impl Site {
    /// A notice for the operator when the site file holds a one-time first sign-in secret that an
    /// earlier version wrote without an expiry. Such a secret is treated as expired; it is never
    /// silently given a lifetime. `None` when there is none.
    pub fn legacy_secret_notice(&self) -> Option<String> {
        let users = self
            .credentials
            .iter()
            .filter(|c| c.first_sign_in && c.expires_at.is_none())
            .count();
        (users > 0).then(|| {
            format!(
                "site `{}`: {users} one-time first sign-in secret(s) have no expiry (written by \
                 an earlier version) and are treated as expired; run `spot reset-admin {} \
                 --yes-i-am-the-operator` or `spot add-user` to issue new ones, or `spot \
                 set-password` to choose a password. Any API key of such a user can only change \
                 the password until then.",
                self.site, self.site
            )
        })
    }
}
fn default_collation() -> String {
    "en".into()
}
fn is_false(value: &bool) -> bool {
    !*value
}
/// At-rest API key and actor id.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    /// Public key id.
    pub key_id: String,
    /// Hex-encoded SHA-256 digest.
    pub digest: String,
    /// User's immutable record id.
    pub user: String,
    /// Unix seconds at which the key stops working. Keys issued by this version always carry
    /// one when they bootstrap an account; an older key without it never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}
/// Fully validated configuration and metadata inputs.
#[derive(Clone)]
pub struct Loaded {
    /// Common file.
    pub common: Common,
    /// Parsed public HTTP settings.
    pub server: ServerConfig,
    /// Administrative connection.
    pub admin: PgConfig,
    /// All sites, sorted by directory name.
    pub sites: Vec<Site>,
}
/// Filesystem-safe lowercase site name, also a valid SiteId.
pub fn site_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && SiteId::new(name).is_ok()
}
/// Database identifiers are intentionally more restrictive than site names.
pub fn database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn check_apps(site: &Site, path: &Path, errors: &mut Vec<String>) {
    if !site.apps.is_empty() {
        let mut seen = BTreeSet::new();
        if site.apps.first().map(String::as_str) != Some(crate::appdir::PLATFORM) {
            errors.push(format!(
                "{}: apps must start with `platform`",
                path.display()
            ));
        }
        for app in &site.apps {
            if !crate::apps::app_name(app) || !seen.insert(app) {
                errors.push(format!("{}: invalid or duplicate app", path.display()));
            }
        }
    }
    let mut users = BTreeSet::new();
    for credential in &site.credentials {
        if spot_http::api::codec::parse_record_id(&credential.user).is_none()
            || spot_auth::PasswordHash::parse(&credential.password_hash).is_err()
            || !users.insert(&credential.user)
        {
            errors.push(format!(
                "{}: invalid or duplicate password verifier",
                path.display()
            ));
        }
    }
}
/// Valid ICU locale token; no SQL syntax is accepted.
pub fn collation(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn parse<T: serde::de::DeserializeOwned>(
    path: &Path,
    schema: &str,
    errors: &mut Vec<String>,
) -> Result<T> {
    let raw = std::fs::read_to_string(path)
        .map_err(|_| Error(format!("cannot read {}", path.display())))?;
    // Parser diagnostics can echo source values; expose location only.
    let mut value: toml::Value = toml::from_str(&raw).map_err(|e: toml::de::Error| {
        Error(format!(
            "invalid TOML in {} at {:?}",
            path.display(),
            e.span()
        ))
    })?;
    crate::decode::clean(&mut value, schema, &path.display().to_string(), errors);
    value
        .try_into()
        .map_err(|_| Error(format!("invalid schema in {}", path.display())))
}
/// Loads every config and aggregates independent validation failures before returning.
pub fn load(root: &Path, dev: bool) -> Result<Loaded> {
    let (loaded, errors) = load_collect(root, dev)?;
    if errors.is_empty() {
        Ok(loaded)
    } else {
        Err(Error(errors.join("\n")))
    }
}
/// Like [`load`], but returns what could be read together with every problem found, so a
/// checker can keep going (for example connect to the sites that did parse). Fails only when
/// `common.toml` cannot be read or parsed at all.
pub fn load_collect(root: &Path, dev: bool) -> Result<(Loaded, Vec<String>)> {
    let mut errors = Vec::new();
    let common: Common = parse(&root.join("common.toml"), "common", &mut errors)?;
    errors.extend(common.backup.problems());
    let allowed = [
        "bind",
        "max_body_bytes",
        "request_timeout",
        "shutdown_grace",
        "trusted_proxies",
        "fallback_site",
        "listeners",
        "auth_rate_limit",
        "auth_rate_window_seconds",
    ];
    if let Some(table) = common.server.as_table() {
        for key in table.keys() {
            if !allowed.contains(&key.as_str()) {
                errors.push(format!("server: unknown field {key}"));
            }
        }
    }
    let server = match common.server.clone().try_into::<ServerConfig>() {
        Ok(c) => c,
        Err(_) => {
            errors.push("server: invalid settings".into());
            ServerConfig::default()
        }
    };
    if server.bind.is_empty() {
        errors.push("server.bind must not be empty".into());
    }
    if server.max_body_bytes == 0 {
        errors.push("server.max_body_bytes must be positive".into());
    }
    if server.request_timeout.is_zero() {
        errors.push("server.request_timeout must be positive".into());
    }
    // `/metrics` without a credential is for a co-located collector. Every bind address serves
    // every route, so one that is reachable from elsewhere would publish the process-wide
    // counters to anyone: the host does not start.
    let metrics = &common.services.metrics;
    if metrics.enabled && !metrics.require_auth && server.bind.iter().any(|a| !a.ip().is_loopback())
    {
        errors.push(
            "services.metrics.require_auth = false needs every server.bind address to be loopback"
                .into(),
        );
    }
    if dev && server.bind.iter().any(|a| !a.ip().is_loopback()) {
        errors.push("--dev requires every bind address to be loopback".into());
    }
    let admin = match PgConfig::from_url(&common.postgres.admin_url) {
        Ok(c) => c,
        Err(_) => {
            errors.push("postgres.admin_url is invalid".into());
            PgConfig::new("", 0, "", "", "")
        }
    };
    if common
        .postgres
        .pool_wait_ms
        .is_some_and(|ms| !(100..=60_000).contains(&ms))
    {
        errors.push("postgres.pool_wait_ms must be 100 to 60000".into());
    }
    if common.postgres.pool_max_size == 0 || common.postgres.pool_max_size > MAX_POOL_MAX_SIZE {
        errors.push(format!(
            "postgres.pool_max_size must be 1 to {MAX_POOL_MAX_SIZE}"
        ));
    }
    if common.apps.is_empty() {
        errors.push("apps must not be empty".into());
    }
    for app in &common.apps {
        if !root.join(app).join("doctypes").is_dir() {
            errors.push(format!(
                "apps: missing doctypes directory in {}",
                app.display()
            ));
        }
    }
    errors.extend(common.services.validate());
    if common
        .telemetry
        .format
        .as_deref()
        .is_some_and(|f| f != "json" && f != "pretty")
    {
        errors.push("telemetry.format must be json or pretty".into());
    }
    if let Some(otlp) = &common.telemetry.otlp {
        if otlp.sample_ppm > 1_000_000 {
            errors.push("telemetry.otlp.sample_ppm must be at most 1000000".into());
        }
        if !(otlp.endpoint.starts_with("http://") || otlp.endpoint.starts_with("https://")) {
            errors.push("telemetry.otlp.endpoint must be an HTTP URL".into());
        }
    }
    let mut sites = Vec::new();
    let mut hosts = BTreeSet::new();
    let mut databases = BTreeSet::new();
    let dir = root.join("sites");
    if dir.exists() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path().join("site.toml");
            match parse::<Site>(&path, "site", &mut errors) {
                Err(e) => errors.push(e.to_string()),
                Ok(site) => {
                    if !site_name(&site.site) || entry.file_name().to_str() != Some(&site.site) {
                        errors.push(format!("{}: invalid site identity", path.display()));
                    }
                    if !database_name(&site.database) || !databases.insert(site.database.clone()) {
                        errors.push(format!("{}: invalid or duplicate database", path.display()));
                    }
                    if !collation(&site.collation) {
                        errors.push(format!("{}: invalid collation", path.display()));
                    }
                    for host in std::iter::once(&site.site).chain(&site.aliases) {
                        if host.is_empty()
                            || host.bytes().any(|b| {
                                !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
                            })
                            || !hosts.insert(host.trim_end_matches('.').to_ascii_lowercase())
                        {
                            errors.push(format!("{}: invalid or duplicate host", path.display()));
                        }
                    }
                    check_apps(&site, &path, &mut errors);
                    if let Some(partition) = &site.partition {
                        for problem in partition.problems() {
                            errors.push(format!("{}: {problem}", path.display()));
                        }
                    }
                    let mut ids = BTreeSet::new();
                    for key in &site.api_keys {
                        if !ids.insert(&key.key_id) || crate::auth::stored(key).is_err() {
                            errors.push(format!(
                                "{}: invalid or duplicate API key verifier",
                                path.display()
                            ));
                        }
                    }
                    sites.push(site);
                }
            }
        }
    }
    if let Some(fallback) = &server.fallback_site
        && !sites.iter().any(|s| s.site == fallback.as_str())
    {
        errors.push("server.fallback_site is not configured".into());
    }
    Ok((
        Loaded {
            common,
            server,
            admin,
            sites,
        },
        errors,
    ))
}

// ---------------------------------------------------------------------------------------------
// Settings the load tests drive (WP-TST-15.T9)
// ---------------------------------------------------------------------------------------------

/// The largest `postgres.pool_max_size` the host accepts. The default (16) is unchanged; the
/// L9 tier profiles set the value they need in `common.toml`.
pub const MAX_POOL_MAX_SIZE: usize = 1_000;

/// The catalogue code of a test hook that was asked for without the explicit opt-in.
pub const TEST_HOOK_REFUSED: &str = "cli.test_hook_refused";

/// The environment variable that must be `1` for the host to accept `--test-hooks`. The flag
/// alone is not enough, so a copied command line cannot turn a production host's clock.
pub const TEST_HOOKS_ENV: &str = "SPOT_TEST_HOOKS";

/// The largest clock offset a test hook accepts, either way: one year.
pub const MAX_CLOCK_OFFSET_SECONDS: i64 = 366 * 24 * 60 * 60;

/// The system clock shifted by a fixed offset: the clock-skew fault of the L9 profiles.
///
/// Only the wall clock is shifted (`now_utc`, which stamps and compares instants). The monotonic
/// reading, which measures intervals, is the system's own, so a skewed node still times its
/// leases and timeouts correctly and only disagrees with other nodes about the date, as a real
/// skewed clock does.
#[derive(Debug)]
pub struct OffsetClock {
    inner: spot_core::SystemClock,
    offset: time::Duration,
}

impl OffsetClock {
    /// A system clock shifted by `seconds` (negative: behind).
    #[must_use]
    pub fn new(seconds: i64) -> Self {
        Self {
            inner: spot_core::SystemClock::new(),
            offset: time::Duration::seconds(seconds),
        }
    }
}

impl spot_core::Clock for OffsetClock {
    fn now_utc(&self) -> time::OffsetDateTime {
        let now = self.inner.now_utc();
        now.checked_add(self.offset).unwrap_or(now)
    }

    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }
}

/// The clock `spot serve` runs with, from its test-hook flags.
///
/// - No offset asked for: `Ok(None)`, the host keeps the system clock.
/// - An offset without `test_hooks`, or with `test_hooks` while `env_allows` is false (the
///   `SPOT_TEST_HOOKS=1` opt-in is missing), or beyond [`MAX_CLOCK_OFFSET_SECONDS`]: refused
///   with [`TEST_HOOK_REFUSED`] and a message that names the flag and never a value.
/// - `test_hooks` alone changes nothing.
///
/// # Errors
///
/// The refusals above.
pub fn clock_from_test_hooks(
    test_hooks: bool,
    env_allows: bool,
    clock_offset: Option<i64>,
) -> Result<Option<std::sync::Arc<dyn spot_core::Clock>>> {
    let Some(seconds) = clock_offset else {
        return Ok(None);
    };
    if !test_hooks {
        return Err(Error(format!(
            "{TEST_HOOK_REFUSED}: --clock-offset is a test hook and needs --test-hooks"
        )));
    }
    if !env_allows {
        return Err(Error(format!(
            "{TEST_HOOK_REFUSED}: --test-hooks needs {TEST_HOOKS_ENV}=1 in the environment; a \
             production host does not set it"
        )));
    }
    if seconds.unsigned_abs() > MAX_CLOCK_OFFSET_SECONDS.unsigned_abs() {
        return Err(Error(format!(
            "{TEST_HOOK_REFUSED}: --clock-offset must be within {MAX_CLOCK_OFFSET_SECONDS} \
             seconds either way"
        )));
    }
    Ok(Some(std::sync::Arc::new(OffsetClock::new(seconds))))
}
