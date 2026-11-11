//! The `cluster-redis` profile hosting (WP-PLT-19.T2): the optional Redis/Valkey plugin of
//! `spot-cluster-redis` offered to the site as the `CacheStore`, `Coordinator` and
//! `InvalidationBus` ports. Operator guide: `docs/running-spot/cluster_redis.md`.
//!
//! Off by default (`enabled = false` registers nothing). Enabled, the module runs at [`ORDER`]
//! 1000 (the registry's minimum for a module file). Credentials are by
//! reference only (`password_ref`), TLS is verified, and plaintext is for loopback.
//!
//! # Failure semantics
//!
//! - **Coordinator:** a lost server answers `coordinator.unavailable`: leases are not renewed
//!   and jobs and the scheduler stop claiming (fail closed). The server checks fencing tokens.
//! - **Cache:** a lost server is a miss; on recovery the site's namespace is cleared (the
//!   generation bumps), so nothing cached before the outage is read.
//! - **Bus:** [`FallbackBus`] falls back to the per-node bus while Redis fails; [`Status`] says so.
//!
//! The start-up check (`PING`, version, clock skew against `TIME`, an `ACL DRYRUN` probe that
//! runs nothing) stops the site's start; see [`startup_check`]. Metrics: see [`metric`].

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use serde::Deserialize;
use spot_cache::CacheStore;
use spot_cluster_redis::{
    DEFAULT_NAMESPACE, RedisBus, RedisCacheStore, RedisConfig, RedisConnection, RedisCoordinator,
};
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, FencingToken, Lease, RateLimit, Result as CoordResult,
};
use spot_core::{Clock, SiteId};
use spot_meta_live::{
    BusEvent, BusFuture, DEFAULT_BUS_CAPACITY, InProcessBus, Invalidation, InvalidationBus,
    InvalidationStream,
};
use tokio::sync::{Notify, watch};
use url::{Host, Url};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order. The registry requires 1000 or more for a module file (`services/mod.rs`), so the
/// ports come after the built-in modules; see the report's open questions.
pub const ORDER: u32 = 1000;

/// Initial connection attempts before the site start is refused.
const CONNECT_ATTEMPTS: u32 = 2;

/// The longest a lease call waits for the server before it counts as lost (fail closed).
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The module and service name, and the `[services.<name>]` table it reads.
pub const NAME: &str = "cluster_redis";

/// Metric names (see the module docs).
pub mod metric {
    /// Gauge: 1 while the server answers, else 0.
    pub const CONNECTED: &str = "spot_cluster_redis_connected";
    /// Histogram: seconds a lease renewal took.
    pub const LEASE_RENEW_SECONDS: &str = "spot_cluster_redis_lease_renew_seconds";
    /// Histogram: seconds the server took to take an invalidation.
    pub const INVALIDATION_PUBLISH_SECONDS: &str =
        "spot_cluster_redis_invalidation_publish_seconds";
    /// Counter: invalidations that fell back to the per-node bus.
    pub const BUS_FALLBACK_TOTAL: &str = "spot_cluster_redis_bus_fallback_total";
    /// Counter: cache namespaces reset after the server came back.
    pub const CACHE_RESETS_TOTAL: &str = "spot_cluster_redis_cache_resets_total";
}

/// `[services.cluster_redis]`. Unknown keys are refused. The URL may not carry credentials, so
/// printing a `Settings` shows no secret.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site coordinates through Redis/Valkey. Off by default.
    pub enabled: bool,
    /// `rediss://host:port` (TLS, verified) or, with `allow_plaintext`, `redis://` on loopback.
    pub url: Option<String>,
    /// Allows `redis://` to a loopback host (development, a sidecar). Never a remote host.
    pub allow_plaintext: bool,
    /// The ACL user name; absent means the server's default user.
    pub username: Option<String>,
    /// Where the password lives: `env:NAME` or `file:/absolute/path`. Never the password.
    pub password_ref: Option<String>,
    /// The key prefix; two clusters on one server use two. Every key also carries the site.
    pub prefix: String,
    /// The connections to hold; only `1` (one multiplexed, reconnecting connection) is accepted.
    pub pool: u32,
    /// Refuse to start when the user may run commands beyond what the plugin needs.
    pub require_restricted_acl: bool,
    /// The most the node's clock may differ from the server's, in milliseconds.
    pub max_clock_skew_ms: u64,
    /// How often the supervisor probes the server, in milliseconds.
    pub probe_interval_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            allow_plaintext: false,
            username: None,
            password_ref: None,
            prefix: DEFAULT_NAMESPACE.to_owned(),
            pool: 1,
            require_restricted_acl: false,
            max_clock_skew_ms: 2_000,
            probe_interval_ms: 2_000,
        }
    }
}

fn refuse(table_key: &str, why: &str) -> Error {
    Error(format!("services.{NAME}.{table_key}: {why}"))
}

/// A secret value. It has no `Display`, and its `Debug` shows nothing.
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Where a password lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordRef {
    /// An environment variable of this process.
    Env(String),
    /// A file, absolute, readable by its owner only.
    File(PathBuf),
}

impl PasswordRef {
    /// Parses `env:NAME` or `file:/absolute/path`.
    ///
    /// # Errors
    ///
    /// Any other shape; the text never quotes the reference.
    pub fn parse(text: &str) -> Result<Self> {
        if let Some(name) = text.strip_prefix("env:") {
            let ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            if !ok {
                return Err(refuse(
                    "password_ref",
                    "an `env:` reference names an upper-case variable (A-Z, 0-9, `_`)",
                ));
            }
            return Ok(Self::Env(name.to_owned()));
        }
        if let Some(path) = text.strip_prefix("file:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(refuse(
                    "password_ref",
                    "a `file:` reference is an absolute path",
                ));
            }
            return Ok(Self::File(path));
        }
        Err(refuse(
            "password_ref",
            "expected `env:NAME` or `file:/absolute/path`; the password itself is never configured",
        ))
    }

    /// Reads the password. A file must not be readable by its group or others (Unix).
    ///
    /// # Errors
    ///
    /// A missing or empty variable or file, or a file others can read. The text names the
    /// variable or the file, never a value.
    pub fn resolve(&self) -> Result<Secret> {
        match self {
            Self::Env(name) => match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(Secret(value)),
                _ => Err(Error(format!(
                    "services.{NAME}.password_ref: the environment variable `{name}` is not set or is empty"
                ))),
            },
            Self::File(path) => {
                check_private(path)?;
                let text = std::fs::read_to_string(path).map_err(|_| unreadable(path))?;
                let value = text.trim_end_matches(['\r', '\n']);
                if value.is_empty() {
                    return Err(Error(format!(
                        "services.{NAME}.password_ref: the file `{}` is empty",
                        path.display()
                    )));
                }
                Ok(Secret(value.to_owned()))
            }
        }
    }
}

fn unreadable(path: &std::path::Path) -> Error {
    Error(format!(
        "services.{NAME}.password_ref: the file `{}` cannot be read",
        path.display()
    ))
}

/// A password file must not be readable by its group or others (checked on Unix).
fn check_private(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|_| unreadable(path))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error(format!(
                "services.{NAME}.password_ref: the file `{}` is readable by its group or others; make it private (mode 0600)",
                path.display()
            )));
        }
    }
    let _ = path;
    Ok(())
}

/// Settings that passed [`validate`].
#[derive(Clone, Debug)]
pub struct Validated {
    pub(crate) url: Url,
    pub(crate) username: Option<String>,
    pub(crate) password_ref: Option<PasswordRef>,
    pub(crate) prefix: String,
    pub(crate) require_restricted_acl: bool,
    pub(crate) max_clock_skew: Duration,
    pub(crate) probe_interval: Duration,
}

impl Validated {
    /// The URL a connection opens: the validated URL with the user and password put in. The
    /// text carries a secret; it goes only into [`RedisConfig`], whose `Debug` redacts it.
    ///
    /// # Errors
    ///
    /// A user or password the URL cannot carry.
    pub fn connection_url(&self, password: Option<&Secret>) -> Result<String> {
        let mut url = self.url.clone();
        if let Some(user) = &self.username {
            url.set_username(user)
                .map_err(|()| refuse("username", "cannot be put into the URL"))?;
        }
        if let Some(secret) = password {
            url.set_password(Some(&secret.0))
                .map_err(|()| refuse("password_ref", "the password cannot be put into the URL"))?;
        }
        Ok(url.into())
    }
}

fn is_loopback(host: &Host<&str>) -> bool {
    match host {
        // A non-special scheme such as `redis` leaves an IPv4 address as a domain text.
        Host::Domain(name) => {
            name.eq_ignore_ascii_case("localhost")
                || name
                    .parse::<std::net::Ipv4Addr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Checks every setting, without touching the network or the environment.
///
/// # Errors
///
/// The first setting that is missing or unsafe, by key name and never by value.
pub fn validate(settings: &Settings) -> Result<Validated> {
    let Some(text) = settings.url.as_deref() else {
        return Err(refuse("url", "is required when the service is enabled"));
    };
    let url = Url::parse(text).map_err(|_| refuse("url", "is not a valid URL"))?;
    // `#insecure` turns certificate verification off in the driver: no option rides the URL.
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(refuse(
            "url",
            "must carry no credentials, query or fragment; use `username` and `password_ref`",
        ));
    }
    let Some(host) = url.host() else {
        return Err(refuse("url", "names no host"));
    };
    match url.scheme() {
        "rediss" => {}
        "redis" => {
            if !settings.allow_plaintext {
                return Err(refuse(
                    "url",
                    "is `redis://`; use `rediss://`, or set allow_plaintext = true for a loopback server",
                ));
            }
            if !is_loopback(&host) {
                return Err(refuse(
                    "url",
                    "is plaintext to a host that is not loopback; plaintext is for loopback only",
                ));
            }
        }
        _ => return Err(refuse("url", "must start with `rediss://`")),
    }
    let prefix = settings.prefix.as_str();
    let prefix_ok = !prefix.is_empty()
        && prefix.len() <= 64
        && prefix.starts_with(|c: char| c.is_ascii_alphanumeric())
        && prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'));
    if !prefix_ok {
        return Err(refuse(
            "prefix",
            "is 1 to 64 characters of letters, digits, `:`, `_` and `-`, starting with a letter or digit",
        ));
    }
    if settings.pool != 1 {
        return Err(refuse(
            "pool",
            "only 1 is supported: the plugin multiplexes one reconnecting connection per site",
        ));
    }
    if let Some(user) = &settings.username
        && (user.is_empty() || user.chars().any(|c| c.is_control() || c.is_whitespace()))
    {
        return Err(refuse("username", "is empty or contains whitespace"));
    }
    if settings.max_clock_skew_ms == 0 || settings.probe_interval_ms == 0 {
        return Err(refuse(
            "max_clock_skew_ms",
            "and probe_interval_ms must be positive",
        ));
    }
    let password_ref = settings
        .password_ref
        .as_deref()
        .map(PasswordRef::parse)
        .transpose()?;
    Ok(Validated {
        url,
        username: settings.username.clone(),
        password_ref,
        prefix: settings.prefix.clone(),
        require_restricted_acl: settings.require_restricted_acl,
        max_clock_skew: Duration::from_millis(settings.max_clock_skew_ms),
        probe_interval: Duration::from_millis(settings.probe_interval_ms),
    })
}

// ---- Start-up check

/// What the ACL probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AclVerdict {
    /// The user may run what the plugin needs and none of the probed extra commands.
    Restricted,
    /// The user may also run these commands (and keys) the plugin does not need.
    Broad(Vec<String>),
    /// The server could not say (`ACL DRYRUN` unavailable or not permitted).
    Unverified(String),
}

/// What the start-up check learned about the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerReport {
    /// The server's version text, when `INFO` answered.
    pub version: Option<String>,
    /// The absolute difference between this node's clock and the server's, in milliseconds.
    pub skew_ms: u64,
    /// The ACL probe's verdict.
    pub acl: AclVerdict,
}

/// A probe command's words.
fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// The commands the plugin runs, on keys under the prefix `{p}`.
const NEEDED: &str = "GET {p}:probe\nSET {p}:probe 1\nDEL {p}:probe\nPUBLISH {p}:bus:probe x\nSCAN 0\nEVALSHA 0000000000000000000000000000000000000000 0";

/// Commands, and a key outside the prefix, that a least-privilege user is refused.
const EXTRA: &str = "FLUSHALL\nFLUSHDB\nCONFIG GET *\nSHUTDOWN\nDEBUG SLEEP 0\nKEYS *\nACL SETUSER spot-probe\nREPLICAOF NO ONE\nGET outside-the-spot-prefix";

async fn run<T: redis::FromRedisValue>(
    connection: &RedisConnection,
    command: &mut redis::Cmd,
) -> std::result::Result<T, redis::RedisError> {
    let mut manager = connection.manager();
    command.query_async::<T>(&mut manager).await
}

fn parse_version(info: &str) -> Option<String> {
    info.lines()
        .find_map(|line| line.trim().strip_prefix("redis_version:"))
        .map(str::to_owned)
}

fn major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

async fn dryrun(
    connection: &RedisConnection,
    user: &str,
    command: &[String],
) -> std::result::Result<bool, String> {
    let mut cmd = redis::cmd("ACL");
    cmd.arg("DRYRUN").arg(user).arg(command);
    let reply = run::<String>(connection, &mut cmd).await;
    reply
        .map(|reply| reply == "OK")
        .map_err(|error| error.to_string())
}

/// The ACL probe: asks the server what the connected user may run. Never runs the probed
/// command itself.
async fn probe_acl(
    connection: &RedisConnection,
    prefix: &str,
) -> std::result::Result<AclVerdict, String> {
    let unverified = |what: &str, why: &str| {
        AclVerdict::Unverified(format!("{what} is refused or unsupported ({})", short(why)))
    };
    let whoami = run::<String>(connection, redis::cmd("ACL").arg("WHOAMI")).await;
    let user = match whoami {
        Ok(user) => user,
        Err(error) => return Ok(unverified("ACL WHOAMI", &error.to_string())),
    };
    for command in NEEDED.replace("{p}", prefix).lines().map(words) {
        match dryrun(connection, &user, &command).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "the ACL of the connected user refuses `{}`, which the plugin needs",
                    command[0]
                ));
            }
            Err(why) => return Ok(unverified("ACL DRYRUN", &why)),
        }
    }
    let mut broad = Vec::new();
    for command in EXTRA.lines().map(words) {
        if dryrun(connection, &user, &command).await == Ok(true) {
            broad.push(command[0].to_ascii_lowercase());
        }
    }
    broad.dedup();
    Ok(if broad.is_empty() {
        AclVerdict::Restricted
    } else {
        AclVerdict::Broad(broad)
    })
}

/// The first line of a server message, bounded, for an operator-facing reason.
fn short(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// The start-up check: ping, version, clock skew and the ACL probe.
///
/// # Errors
///
/// The reason the server cannot be used: it does not answer, is older than
/// 6, its clock differs by more than the allowed skew, it will not tell its
/// time, the user's ACL refuses a command the plugin needs, or (when
/// `require_restricted_acl`) the user may run commands it does not need.
pub async fn startup_check(
    connection: &RedisConnection,
    clock: &dyn Clock,
    settings: &Validated,
) -> std::result::Result<ServerReport, String> {
    connection.ping().await.map_err(|error| {
        format!(
            "the server does not answer PING ({})",
            short(&error.to_string())
        )
    })?;
    let info = run::<String>(connection, redis::cmd("INFO").arg("server")).await;
    let version = info.ok().and_then(|info| parse_version(&info));
    if let Some(version) = &version
        && major(version).is_some_and(|m| m < 6)
    {
        return Err(format!(
            "the server version {version} is too old (the plugin needs 6 or newer)"
        ));
    }
    // Leases decide who runs singleton work from clocks; a node far from the server must not join.
    let (secs, micros) = run::<(i64, i64)>(connection, &mut redis::cmd("TIME"))
        .await
        .map_err(|error| {
            format!(
                "the server will not tell its time, so the clock skew cannot be checked ({})",
                short(&error.to_string())
            )
        })?;
    let server_us = i128::from(secs) * 1_000_000 + i128::from(micros);
    let local_us = clock.now_utc().unix_timestamp_nanos() / 1_000;
    let skew_ms = u64::try_from((server_us - local_us).unsigned_abs() / 1_000).unwrap_or(u64::MAX);
    if u128::from(skew_ms) > settings.max_clock_skew.as_millis() {
        return Err(format!(
            "this node's clock differs from the server's by {skew_ms} ms, more than the allowed {} ms",
            settings.max_clock_skew.as_millis()
        ));
    }
    let acl = probe_acl(connection, &settings.prefix).await?;
    if let (AclVerdict::Broad(commands), true) = (&acl, settings.require_restricted_acl) {
        return Err(format!(
            "the connected user may also run commands the plugin does not need ({}); restrict its ACL or unset require_restricted_acl",
            commands.join(", ")
        ));
    }
    Ok(ServerReport {
        version,
        skew_ms,
        acl,
    })
}

// ---- Status and observation

/// What the plugin is doing right now, for the host's readiness and for tests.
#[derive(Debug)]
pub struct Status {
    connected: AtomicBool,
    bus_degraded: AtomicBool,
    fallbacks: AtomicU64,
    cache_resets: AtomicU64,
}

impl Status {
    /// A status that reports a connected, undegraded node.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connected: AtomicBool::new(true),
            bus_degraded: AtomicBool::new(false),
            fallbacks: AtomicU64::new(0),
            cache_resets: AtomicU64::new(0),
        })
    }

    /// Whether the server answered the last call or probe.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Whether invalidations go through the per-node bus because Redis refused them.
    #[must_use]
    pub fn bus_degraded(&self) -> bool {
        self.bus_degraded.load(Ordering::SeqCst)
    }

    /// Invalidations that fell back to the per-node bus so far.
    #[must_use]
    pub fn bus_fallbacks(&self) -> u64 {
        self.fallbacks.load(Ordering::SeqCst)
    }

    /// Cache namespaces reset after the server came back, so far.
    #[must_use]
    pub fn cache_resets(&self) -> u64 {
        self.cache_resets.load(Ordering::SeqCst)
    }

    /// Why the node is degraded, or `None` when it is not: the line readiness shows.
    #[must_use]
    pub fn degraded_reason(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.connected() {
            parts.push("the coordination server is unreachable; leases cannot be renewed and jobs and the scheduler do not claim");
        }
        if self.bus_degraded() {
            parts.push("realtime and metadata invalidations use the per-node bus only");
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    fn set_connected(&self, up: bool) -> bool {
        let was = self.connected.swap(up, Ordering::SeqCst);
        metrics::gauge!(metric::CONNECTED).set(if up { 1.0 } else { 0.0 });
        was
    }
}

/// A [`Coordinator`] decorator: times lease renewals and tells [`Status`] whether the server
/// answers. It never changes an outcome, so a lost server stays `coordinator.unavailable`.
pub struct ObservedCoordinator {
    inner: Arc<dyn Coordinator>,
    status: Arc<Status>,
    clock: Arc<dyn Clock>,
}

impl ObservedCoordinator {
    /// Observes `inner`.
    pub fn new(inner: Arc<dyn Coordinator>, status: Arc<Status>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            status,
            clock,
        }
    }

    fn note<T>(&self, result: &CoordResult<T>) {
        // Any answer, even a refusal, proves the server is reachable.
        let down =
            matches!(result, Err(e) if e.code == spot_cluster_redis::coordinator::UNAVAILABLE);
        self.status.set_connected(!down);
    }
}

/// Awaits a lease call for at most [`CALL_TIMEOUT`]; a server that hangs is a lost server.
async fn bounded<T>(call: BoxFuture<'_, CoordResult<T>>) -> CoordResult<T> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(spot_coordinator::Error::new(
                spot_cluster_redis::coordinator::UNAVAILABLE,
                "the coordination server did not answer in time",
            ))
        })
}

impl Coordinator for ObservedCoordinator {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        Box::pin(async move {
            let result = bounded(self.inner.acquire(name, holder, ttl)).await;
            self.note(&result);
            result
        })
    }

    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        Box::pin(async move {
            let started = self.clock.monotonic();
            let result = bounded(self.inner.renew(lease, ttl)).await;
            let took = self.clock.monotonic().saturating_sub(started);
            metrics::histogram!(metric::LEASE_RENEW_SECONDS).record(took.as_secs_f64());
            self.note(&result);
            result
        })
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }

    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }

    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        Box::pin(async move {
            let result = bounded(self.inner.check_fence(name, token)).await;
            self.note(&result);
            result
        })
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.peek(name)
    }

    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.inner.take(key, limit, cost)
    }

    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }

    fn release_now(&self, lease: &Lease) -> bool {
        self.inner.release_now(lease)
    }
}

/// An [`InvalidationBus`] over Redis that falls back to the per-node bus.
///
/// `publish` goes to the server; when the server refuses it, the change goes to this node's own
/// subscribers instead and [`Status::bus_degraded`] says so. `subscribe` merges the server's
/// stream (which reports a `Gap` after a reconnect, so a subscriber reconciles) with the
/// per-node one. While the server answers, only the server carries a change, so a subscriber
/// hears it once.
pub struct FallbackBus {
    primary: Arc<dyn InvalidationBus>,
    local: InProcessBus,
    status: Arc<Status>,
    clock: Arc<dyn Clock>,
}

impl FallbackBus {
    /// A bus over `primary`.
    pub fn new(
        primary: Arc<dyn InvalidationBus>,
        status: Arc<Status>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            primary,
            local: InProcessBus::new(DEFAULT_BUS_CAPACITY),
            status,
            clock,
        }
    }
}

impl InvalidationBus for FallbackBus {
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()> {
        Box::pin(async move {
            let started = self.clock.monotonic();
            match self.primary.publish(site, invalidation.clone()).await {
                Ok(()) => {
                    let took = self.clock.monotonic().saturating_sub(started);
                    metrics::histogram!(metric::INVALIDATION_PUBLISH_SECONDS)
                        .record(took.as_secs_f64());
                    self.status.bus_degraded.store(false, Ordering::SeqCst);
                    Ok(())
                }
                Err(_) => {
                    self.status.bus_degraded.store(true, Ordering::SeqCst);
                    self.status.fallbacks.fetch_add(1, Ordering::SeqCst);
                    metrics::counter!(metric::BUS_FALLBACK_TOTAL).increment(1);
                    self.local.publish(site, invalidation).await
                }
            }
        })
    }

    fn subscribe(&self, site: &SiteId) -> InvalidationStream {
        Box::pin(Merged {
            first: self.primary.subscribe(site),
            second: self.local.subscribe(site),
            first_done: false,
            second_done: false,
            turn: false,
        })
    }
}

/// Two streams as one, polled in turn so neither starves the other.
struct Merged {
    first: InvalidationStream,
    second: InvalidationStream,
    first_done: bool,
    second_done: bool,
    turn: bool,
}

impl Stream for Merged {
    type Item = BusEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BusEvent>> {
        let this = self.get_mut();
        this.turn = !this.turn;
        for round in 0..2 {
            let first_now = this.turn == (round == 0);
            let (stream, done) = if first_now {
                (&mut this.first, &mut this.first_done)
            } else {
                (&mut this.second, &mut this.second_done)
            };
            if *done {
                continue;
            }
            match stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(event)) => return Poll::Ready(Some(event)),
                Poll::Ready(None) => *done = true,
                Poll::Pending => {}
            }
        }
        if this.first_done && this.second_done {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

// ---- The service and the assembly

/// The supervisor: probes the server on an interval, keeps [`Status`] and the gauge current and,
/// when the server comes back after an outage, resets the site's cache namespace.
pub struct ClusterRedis {
    connection: RedisConnection,
    site: SiteId,
    cache: Arc<dyn CacheStore>,
    status: Arc<Status>,
    interval: Duration,
}

impl ClusterRedis {
    /// One probe: records the outcome and, on a recovery, resets the cache namespace.
    pub async fn probe_once(&self) {
        let up = self.connection.ping().await.is_ok();
        let was = self.status.set_connected(up);
        if up && !was {
            // Anything cached before the outage may be stale: bump the generation.
            self.cache.clear_site(&self.site).await;
            self.status.cache_resets.fetch_add(1, Ordering::SeqCst);
            metrics::counter!(metric::CACHE_RESETS_TOTAL).increment(1);
            tracing::info!(site = %self.site, "coordination server is back; cache namespace reset");
        } else if !up && was {
            tracing::warn!(site = %self.site, "coordination server is unreachable; leases cannot be renewed");
        }
    }
}

impl Service for ClusterRedis {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        // The site keeps serving reads without it; the jobs and scheduler fail closed through the
        // coordinator, and `Status` says what is degraded.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.connection
                .ping()
                .await
                .map_err(|_| "the coordination server does not answer".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(self.interval) => self.probe_once().await,
                }
            }
        })
    }
}

/// What a site gets from an enabled plugin.
pub struct Parts {
    /// The coordinator port.
    pub coordinator: Arc<dyn Coordinator>,
    /// The cache port.
    pub cache: Arc<dyn CacheStore>,
    /// The invalidation bus port.
    pub bus: Arc<dyn InvalidationBus>,
    /// What the plugin is doing.
    pub status: Arc<Status>,
    /// The supervisor service.
    pub service: Arc<ClusterRedis>,
    /// What the start-up check learned.
    pub report: ServerReport,
}

/// Connects, runs the start-up check and builds the ports for `site`.
///
/// # Errors
///
/// The password reference does not resolve, the server cannot be reached, or the start-up check
/// refuses it. Nothing falls back to another coordinator: a site that asked for Redis does not
/// start without it.
pub async fn build(settings: &Settings, site: &SiteId, clock: Arc<dyn Clock>) -> Result<Parts> {
    let validated = validate(settings)?;
    let password = validated
        .password_ref
        .as_ref()
        .map(PasswordRef::resolve)
        .transpose()?;
    let url = validated.connection_url(password.as_ref())?;
    drop(password);
    // The driver already retries inside each attempt; a few attempts keep a failed start short.
    let config = RedisConfig::new(url)
        .with_connect_attempts(CONNECT_ATTEMPTS, Duration::from_millis(100))
        .with_keyspace(spot_cluster_redis::KeySpace::with_prefix(
            validated.prefix.clone(),
        ));
    let connection = RedisConnection::connect(&config).await.map_err(|error| {
        Error(format!(
            "services.{NAME}: cannot connect to the server ({})",
            short(&error.to_string())
        ))
    })?;
    let report = startup_check(&connection, clock.as_ref(), &validated)
        .await
        .map_err(|why| Error(format!("services.{NAME}: start-up check failed: {why}")))?;
    if let AclVerdict::Broad(commands) = &report.acl {
        tracing::warn!(site = %site, commands = %commands.join(","), "the coordination user may run more than the plugin needs");
    }
    let status = Status::new();
    status.set_connected(true);
    let coordinator: Arc<dyn Coordinator> = Arc::new(ObservedCoordinator::new(
        Arc::new(RedisCoordinator::new(
            connection.clone(),
            Arc::clone(&clock),
        )),
        Arc::clone(&status),
        Arc::clone(&clock),
    ));
    let cache: Arc<dyn CacheStore> = Arc::new(RedisCacheStore::with_default(connection.clone()));
    let bus: Arc<dyn InvalidationBus> = Arc::new(FallbackBus::new(
        Arc::new(RedisBus::new(connection.clone())),
        Arc::clone(&status),
        Arc::clone(&clock),
    ));
    let service = Arc::new(ClusterRedis {
        connection,
        site: site.clone(),
        cache: Arc::clone(&cache),
        status: Arc::clone(&status),
        interval: validated.probe_interval,
    });
    Ok(Parts {
        coordinator,
        cache,
        bus,
        status,
        service,
        report,
    })
}

/// Registers the plugin: nothing when disabled; otherwise the `Coordinator`, `CacheStore` and
/// `InvalidationBus` ports, the [`Status`] and the supervisor service.
///
/// # Errors
///
/// A setting that does not decode or validate, or anything [`build`] refuses.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    if !ctx.assembling() || !settings.enabled {
        return Ok(());
    }
    // A bad setting is refused before anything else, even where no site is attached.
    validate(&settings)?;
    let site = SiteId::new(&ctx.env()?.site.site).map_err(|e| Error(e.to_string()))?;
    let parts = build(&settings, &site, Arc::clone(ctx.clock())).await?;
    ctx.provide(parts.coordinator)?;
    ctx.provide(parts.cache)?;
    ctx.provide(parts.bus)?;
    ctx.provide(parts.status)?;
    ctx.add(parts.service);
    Ok(())
}
