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
/// The identity audit trail.
///
/// A site with a PostgreSQL database writes its identity events to the hash-chained audit store;
/// these options only say what is written *beside* the chain and how loudly a broken chain is
/// reported. The defaults keep the file log off and readiness warning-only.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audit {
    /// Also append every identity record to `sites/<site>/identity-audit.log` (mode 0600, size
    /// capped), after the chain append. The file is a convenience for log shippers and is never
    /// relied on: its failure does not fail an append.
    pub identity_file_mirror: bool,
    /// Make `/api/health/ready` fail (not only warn) while the identity chain does not verify.
    pub identity_chain_strict: bool,
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
}
impl Default for Realtime {
    fn default() -> Self {
        Self {
            enabled: true,
            heartbeat_seconds: 25,
            poll_ms: 100,
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
            queues: vec!["default".into()],
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
    /// The SMTP account, required when `sink = "smtp"`. The password is a secret and is
    /// never included in an error, a log line or the readiness report.
    pub smtp: Option<Smtp>,
}
impl Mail {
    /// Whether the sent log lives in PostgreSQL for a site whose job store is `job_store`:
    /// `auto` follows the job store, the others say so themselves.
    #[must_use]
    pub fn sent_log_in_postgres(&self, job_store: &str) -> bool {
        match self.sent_log.as_str() {
            "postgres" => true,
            "memory" => false,
            _ => job_store == "postgres",
        }
    }
}

impl Default for Mail {
    fn default() -> Self {
        Self {
            enabled: true,
            sink: "memory".into(),
            sent_log: "auto".into(),
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
    /// `local` (bytes under [`Files::root`]) or `memory` (tests).
    pub provider: String,
    /// The local provider's root, relative to the site directory.
    pub root: String,
    /// Hard per-upload limit in bytes.
    pub max_bytes: u64,
}
impl Default for Files {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            root: "files".into(),
            max_bytes: 25 * 1024 * 1024,
        }
    }
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
        if !matches!(self.files.provider.as_str(), "local" | "memory") {
            errors.push("services.files.provider must be local or memory".into());
        }
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
    let allowed = [
        "bind",
        "max_body_bytes",
        "request_timeout",
        "shutdown_grace",
        "trusted_proxies",
        "fallback_site",
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
    if common.postgres.pool_max_size == 0 {
        errors.push("postgres.pool_max_size must be positive".into());
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
