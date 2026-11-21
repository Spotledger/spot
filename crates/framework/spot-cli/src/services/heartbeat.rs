//! The fleet heartbeat emitter (WP-CTL-03.T3, ADR-061): the optional tenant-host service that
//! samples the closed set of fleet-health signals and ships them to the control plane
//! (`POST /cp/v1/host/heartbeat`) over mutual TLS. **Off by default**
//! (`[services.heartbeat] enabled = false`): a desktop tenant without a control plane makes no
//! network call and keeps working.
//!
//! # What it is, and what it is not
//!
//! Every `interval_seconds` (default 15, at least 5) the emitter's own task runs the samplers of
//! [`sample`], writes one batch to a **file outbox** ([`outbox`], in the data directory, not in the
//! database: the site may be the thing that is down) and ships what waits, oldest first. Nothing
//! here is called by a request; samplers read atomics and cheap host facts, each within a time
//! budget, so a slow or failing sampler can neither delay a request nor stop the next heartbeat.
//!
//! # Silence means "host down"
//!
//! A heartbeat is sent even when every sampler fails: `health_probe = 0`, with the failing
//! samplers left out (their names are not part of the allow-list and are never sent). The control
//! plane then shows a red host that is talking, which is different from a host that has stopped.
//!
//! # Offline is normal
//!
//! A shipment that gets no answer, a 429 or a 5xx stays queued and is retried with exponential
//! backoff and jitter (injected). A full outbox drops its **oldest** batches and keeps one local
//! gap marker, so the newest observations survive; an outage of days on a desktop tenant is fine.
//! A refusal that retrying cannot fix (400, 413, 422) drops that batch and counts it. A full outbox
//! is a degraded state of this optional service and a counter, never an error to a user.
//!
//! # No business data
//!
//! The payload is `{"observations": [{scope, signal, value, observed_at}]}`: a closed signal name,
//! an integer, a scope word and a time. The host and the tenant are named by the client certificate,
//! not by the body, so a batch cannot speak for another tenant. The transport is outbound only;
//! the key file mode is checked ([`check_credentials`]); the control-plane address must be on the
//! site's egress allow-list and the start-up message names the missing entry.
//!
//! Nothing here reads the wall clock or draws randomness (both are injected) or uses floats.

#[path = "heartbeat/outbox.rs"]
pub mod outbox;
#[path = "heartbeat/sample.rs"]
pub mod sample;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_egress::{EgressConnector, HttpRequest, Method, Sender};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

use self::outbox::FileOutbox;
use self::sample::{Reading, Scope, SignalSampler, collect};

/// Start order: late, after the services whose numbers it samples (metering is 1700).
pub const ORDER: u32 = 1710;

/// Bytes per batch the control plane accepts.
pub const MAX_BODY: usize = 256 * 1024;
/// Observations per batch the control plane accepts.
pub const MAX_OBSERVATIONS: usize = 200;
/// The control plane's endpoint.
pub const ENDPOINT: &str = "/cp/v1/host/heartbeat";

/// Stable codes of this service's start-up refusals (catalogue `heartbeat_host.toml`).
pub mod code {
    /// A credential file is missing, unreadable or the key is group or world readable.
    pub const CREDENTIAL_FILE: &str = "heartbeat_host.credential_file";
    /// The control plane is not on the egress allow-list.
    pub const EGRESS_NOT_ALLOWED: &str = "heartbeat_host.egress_not_allowed";
    /// A setting is out of range.
    pub const INVALID_SETTING: &str = "heartbeat_host.invalid_setting";
    /// Enabled, but the host provided no ports.
    pub const NO_PORTS: &str = "heartbeat_host.no_ports";
}

/// Metric names.
pub mod metric {
    /// Counter: heartbeat batches the control plane accepted.
    pub const SHIPPED_TOTAL: &str = "spot_heartbeat_shipped_total";
    /// Counter: failed shipments or writes, by `reason` (`unreachable`, `refused`, `outbox`).
    pub const FAILED_TOTAL: &str = "spot_heartbeat_failed_total";
    /// Counter: batches dropped because the outbox was full.
    pub const DROPPED_TOTAL: &str = "spot_heartbeat_dropped_total";
}

/// `[services.heartbeat]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the emitter runs. Off by default.
    pub enabled: bool,
    /// The control plane's host listener, `https://host[:port]` (no path).
    pub control_plane: String,
    /// This host's client certificate issued by the control plane's host CA (PEM).
    pub host_cert: String,
    /// The client key (PEM), mode 0600.
    pub host_key: String,
    /// The CA that signed the control plane's server certificate (PEM).
    pub ca_file: String,
    /// Seconds between heartbeats. At least 5.
    pub interval_seconds: u64,
    /// A label for this host in log lines (not sent: the certificate names the host).
    pub node_id: String,
    /// The time one sampler may take, milliseconds. At least 50.
    pub sample_budget_ms: u64,
    /// Batches the outbox holds before it drops the oldest. At least 4.
    pub outbox_max_batches: usize,
    /// The outbox directory; empty means `heartbeat-outbox` in the data directory.
    pub outbox_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            control_plane: String::new(),
            host_cert: String::new(),
            host_key: String::new(),
            ca_file: String::new(),
            interval_seconds: 15,
            node_id: String::new(),
            sample_budget_ms: 2_000,
            outbox_max_batches: 5_760,
            outbox_dir: String::new(),
        }
    }
}

fn invalid(name: &str, rule: &str) -> Error {
    Error(format!(
        "{}: services.heartbeat.{name} {rule}",
        code::INVALID_SETTING
    ))
}

impl Settings {
    /// Checks the ranges of an enabled configuration.
    ///
    /// # Errors
    /// A message naming the setting, never a value.
    pub fn validate(&self) -> Result<()> {
        self.control_plane_endpoint()?;
        if self.host_cert.is_empty() || self.host_key.is_empty() || self.ca_file.is_empty() {
            return Err(invalid("host_cert", "host_key and ca_file are required"));
        }
        if self.interval_seconds < 5 {
            return Err(invalid("interval_seconds", "must be at least 5"));
        }
        if self.sample_budget_ms < 50 {
            return Err(invalid("sample_budget_ms", "must be at least 50"));
        }
        if self.outbox_max_batches < 4 {
            return Err(invalid("outbox_max_batches", "must be at least 4"));
        }
        let node_ok = !self.node_id.is_empty()
            && self.node_id.len() <= 128
            && self
                .node_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
        if !node_ok {
            return Err(invalid(
                "node_id",
                "must be 1 to 128 characters of A-Za-z0-9-_.:",
            ));
        }
        Ok(())
    }

    /// The control plane's `(host, port)`; the port defaults to 443.
    ///
    /// # Errors
    /// A message naming the setting when it is not an `https://host[:port]` URL.
    pub fn control_plane_endpoint(&self) -> Result<(String, u16)> {
        let bad = || invalid("control_plane", "must be an https://host[:port] URL");
        let rest = self
            .control_plane
            .strip_prefix("https://")
            .ok_or_else(bad)?;
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') || rest.contains('@') || rest.contains('?') {
            return Err(bad());
        }
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && !host.contains(':') => {
                let port: u16 = port.parse().map_err(|_| bad())?;
                Ok((host.to_owned(), port))
            }
            Some(_) => Err(bad()),
            None => Ok((rest.to_owned(), 443)),
        }
    }

    /// The control plane's URL for the endpoint.
    #[must_use]
    pub fn endpoint_url(&self) -> String {
        format!("{}{ENDPOINT}", self.control_plane.trim_end_matches('/'))
    }
}

/// Checks the credential files: they exist and are readable, and the private key is not readable
/// by group or others.
///
/// # Errors
/// A message that names the file (a path is configuration, not a secret) and never its content.
pub fn check_credentials(settings: &Settings) -> std::result::Result<(), String> {
    for (name, path) in [
        ("host_cert", &settings.host_cert),
        ("host_key", &settings.host_key),
        ("ca_file", &settings.ca_file),
    ] {
        if std::fs::File::open(path).is_err() || !Path::new(path).is_file() {
            return Err(format!(
                "{}: services.heartbeat.{name} ({path}) does not name a readable file",
                code::CREDENTIAL_FILE
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&settings.host_key)
            .map_err(|_| {
                format!(
                    "{}: services.heartbeat.host_key ({}) cannot be read",
                    code::CREDENTIAL_FILE,
                    settings.host_key
                )
            })?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{}: services.heartbeat.host_key ({}) must be mode 0600 or stricter",
                code::CREDENTIAL_FILE,
                settings.host_key
            ));
        }
    }
    Ok(())
}

// ---- wire ----------------------------------------------------------------------------------

#[derive(Serialize)]
struct WireItem {
    scope: Scope,
    signal: sample::Signal,
    value: i64,
    observed_at: i64,
}

#[derive(Serialize)]
struct WireBatch {
    observations: Vec<WireItem>,
}

/// The JSON body of one batch: `readings` observed at `at` (Unix seconds). At most
/// [`MAX_OBSERVATIONS`] readings are written (more cannot exist: the signal set is closed).
#[must_use]
pub fn wire_body(readings: &[Reading], at: i64) -> Vec<u8> {
    let batch = WireBatch {
        observations: readings
            .iter()
            .take(MAX_OBSERVATIONS)
            .map(|r| WireItem {
                scope: r.scope,
                signal: r.signal,
                value: r.value,
                observed_at: at,
            })
            .collect(),
    };
    serde_json::to_vec(&batch).unwrap_or_default()
}

// ---- shipping ------------------------------------------------------------------------------

/// What the control plane did with a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShipOutcome {
    /// Accepted (2xx).
    Accepted,
    /// Refused for good (400, 413, 422): retrying the same batch cannot help.
    Refused,
    /// No answer, 429, 5xx, or a refusal about the host rather than the batch (401, 403, 404).
    Retry,
}

/// Sends one batch.
#[async_trait]
pub trait HeartbeatShipper: Send + Sync {
    /// Posts `body` to the heartbeat endpoint.
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome;
}

/// Maps an HTTP status to what the emitter does next.
#[must_use]
pub fn outcome_of(status: u16) -> ShipOutcome {
    match status {
        200..=299 => ShipOutcome::Accepted,
        400 | 413 | 422 => ShipOutcome::Refused,
        _ => ShipOutcome::Retry,
    }
}

/// A shipper over the shared egress guard: it authorises the control-plane URL (so a missing
/// allow-list entry is refused and recorded like any denied call), then sends through `sender`,
/// which the host builds as an `HttpSender::with_tls` over the host certificate, key and CA
/// (WP-SEC-09.T8). Redirects are not followed: a control plane never redirects a heartbeat.
pub struct EgressShipper {
    connector: EgressConnector,
    sender: Arc<dyn Sender>,
    url: url::Url,
}

impl EgressShipper {
    /// A shipper posting to `settings`' endpoint.
    ///
    /// # Errors
    /// A message when the endpoint is not a valid URL.
    pub fn new(
        connector: EgressConnector,
        sender: Arc<dyn Sender>,
        settings: &Settings,
    ) -> Result<Self> {
        let url = url::Url::parse(&settings.endpoint_url())
            .map_err(|_| invalid("control_plane", "must be an https://host[:port] URL"))?;
        Ok(Self {
            connector,
            sender,
            url,
        })
    }
}

#[async_trait]
impl HeartbeatShipper for EgressShipper {
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome {
        let Ok(authorized) = self.connector.authorize(&self.url).await else {
            return ShipOutcome::Retry;
        };
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        let request = HttpRequest::new(
            Method::Post,
            self.url.clone(),
            headers,
            Some(spot_egress::Bytes::from(body)),
        );
        match self
            .sender
            .send_request(&request, &authorized, 16 * 1024)
            .await
        {
            Ok(response) => outcome_of(response.status.as_u16()),
            Err(_) => ShipOutcome::Retry,
        }
    }
}

/// Whether the site's egress policy lets the control plane through. The host implements it over
/// its guard's tenant policy.
pub trait EgressAllowList: Send + Sync {
    /// Whether `host:port` is allowed for this emitter.
    fn allows(&self, host: &str, port: u16) -> bool;
}

/// Where randomness for the backoff jitter comes from (injected: the emitter draws none itself).
pub trait Jitter: Send + Sync {
    /// A value in `0..1000`.
    fn permille(&self) -> u64;
}

/// No jitter (tests, and hosts that prefer a fixed delay).
pub struct NoJitter;

impl Jitter for NoJitter {
    fn permille(&self) -> u64 {
        0
    }
}

/// The delay before the next attempt after `attempt` consecutive failures: `base` doubled per
/// failure up to `cap`, then spread over its upper half by `jitter_permille` (0 to 999).
#[must_use]
pub fn backoff_secs(base: u64, attempt: u32, cap: u64, jitter_permille: u64) -> u64 {
    let raw = base
        .saturating_mul(1_u64 << attempt.min(16))
        .min(cap.max(base));
    let half = raw / 2;
    half + half.saturating_mul(jitter_permille.min(999)) / 1000
}

// ---- the service ---------------------------------------------------------------------------

/// What the emitter needs from the rest of the host. Provide one with
/// `SiteCtx::provide::<HeartbeatPorts>`; enabled with no provider the site refuses to start.
pub struct HeartbeatPorts {
    /// The samplers the site can honestly offer (see [`sample`]); `health_probe` is added as
    /// `0` when none is a health-probe sampler.
    pub samplers: Vec<Arc<dyn SignalSampler>>,
    /// The mutual-TLS shipper (usually an [`EgressShipper`]).
    pub shipper: Arc<dyn HeartbeatShipper>,
    /// The egress allow-list, checked once at start.
    pub egress: Arc<dyn EgressAllowList>,
    /// Backoff jitter.
    pub jitter: Arc<dyn Jitter>,
    /// The site's data directory (the outbox lives under it unless `outbox_dir` is set).
    pub data_dir: PathBuf,
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Samplers that failed or ran over budget.
    pub sampler_failures: usize,
    /// Batches the control plane accepted.
    pub shipped: usize,
    /// Batches dropped because the control plane refused them for good.
    pub refused: usize,
    /// Batches dropped because the outbox was full.
    pub dropped: u64,
    /// Batches still waiting.
    pub waiting: usize,
    /// Whether the control plane answered (false after a retryable failure).
    pub reached: bool,
}

/// The emitter.
pub struct HeartbeatService {
    ports: Arc<HeartbeatPorts>,
    settings: Settings,
    clock: Arc<dyn Clock>,
    outbox: FileOutbox,
    degraded: AtomicBool,
    failures: AtomicU32,
}

fn bump(name: &'static str) {
    metrics::counter!(name).increment(1);
}

fn bump_failed(reason: &'static str) {
    metrics::counter!(metric::FAILED_TOTAL, "reason" => reason).increment(1);
}

impl HeartbeatService {
    /// An emitter over `ports`, reading time from `clock`.
    ///
    /// # Errors
    /// A message when the outbox directory cannot be opened.
    pub fn new(
        ports: Arc<HeartbeatPorts>,
        settings: Settings,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let dir = if settings.outbox_dir.is_empty() {
            ports.data_dir.join("heartbeat-outbox")
        } else {
            PathBuf::from(&settings.outbox_dir)
        };
        let outbox = FileOutbox::open(&dir, settings.outbox_max_batches)
            .map_err(|message| Error(format!("services.heartbeat: {message}")))?;
        Ok(Self {
            ports,
            settings,
            clock,
            outbox,
            degraded: AtomicBool::new(false),
            failures: AtomicU32::new(0),
        })
    }

    /// Whether the outbox is full (the oldest batches are being dropped).
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// The outbox, for readiness and tests.
    #[must_use]
    pub fn outbox(&self) -> &FileOutbox {
        &self.outbox
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// One pass at `now` (Unix seconds): sample, queue one batch, ship what waits. Never panics;
    /// a failing sampler, outbox or endpoint is counted and the pass reports what it did.
    pub async fn pass_at(&self, now: i64) -> PassReport {
        let budget = Duration::from_millis(self.settings.sample_budget_ms);
        let collected = collect(&self.ports.samplers, budget).await;
        let mut report = PassReport {
            sampler_failures: collected.failed,
            ..PassReport::default()
        };
        for _ in 0..collected.failed {
            bump_failed("sampler");
        }
        let body = wire_body(&collected.readings, now);
        match self.outbox.push(&body, now) {
            Ok(pushed) => {
                report.dropped = pushed.dropped;
                for _ in 0..pushed.dropped {
                    bump(metric::DROPPED_TOTAL);
                }
                self.degraded.store(pushed.full, Ordering::SeqCst);
            }
            Err(_) => bump_failed("outbox"),
        }
        self.ship(&mut report).await;
        report.waiting = self.outbox.len().unwrap_or(0);
        if report.waiting < self.settings.outbox_max_batches {
            self.degraded.store(false, Ordering::SeqCst);
        }
        report
    }

    /// One pass now.
    pub async fn pass(&self) -> PassReport {
        self.pass_at(self.now()).await
    }

    async fn ship(&self, report: &mut PassReport) {
        report.reached = true;
        while let Ok(Some((seq, body))) = self.outbox.oldest() {
            debug_assert!(body.len() <= MAX_BODY);
            match self.ports.shipper.ship(body).await {
                ShipOutcome::Accepted => {
                    if self.outbox.remove(seq).is_err() {
                        bump_failed("outbox");
                        break;
                    }
                    self.outbox.clear_gap();
                    bump(metric::SHIPPED_TOTAL);
                    report.shipped += 1;
                    self.failures.store(0, Ordering::SeqCst);
                }
                ShipOutcome::Refused => {
                    if self.outbox.remove(seq).is_err() {
                        break;
                    }
                    bump_failed("refused");
                    report.refused += 1;
                }
                // The batch stays; the next pass tries again after a backoff. Offline for days
                // is normal for a desktop tenant.
                ShipOutcome::Retry => {
                    bump_failed("unreachable");
                    self.failures.fetch_add(1, Ordering::SeqCst);
                    report.reached = false;
                    break;
                }
            }
        }
    }

    /// The pause before the next pass: the interval, or a backoff after failures.
    #[must_use]
    pub fn next_delay(&self) -> Duration {
        let failures = self.failures.load(Ordering::SeqCst);
        let interval = self.settings.interval_seconds;
        if failures == 0 {
            return Duration::from_secs(interval);
        }
        let cap = interval.saturating_mul(20).max(300);
        let wait = backoff_secs(
            interval,
            failures.saturating_sub(1),
            cap,
            self.ports.jitter.permille(),
        );
        Duration::from_secs(wait.max(interval))
    }
}

impl Service for HeartbeatService {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    /// Optional: losing the heartbeat degrades the site's readiness text and nothing else.
    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            check_credentials(&self.settings)?;
            if self.degraded() {
                return Err(
                    "the heartbeat outbox is full; the oldest batches are being dropped".to_owned(),
                );
            }
            self.outbox.len().map(|_| ())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                self.pass().await;
                tokio::select! {
                    () = tokio::time::sleep(self.next_delay()) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
            }
        })
    }
}

/// Registers the emitter when `[services.heartbeat]` is enabled. Disabled (the default), nothing
/// is registered: no task, no directory, no network call.
///
/// # Errors
/// A setting out of range, a missing or unsafe credential file, a control plane that is not on
/// the egress allow-list (the message names the `host:port` entry to add), or missing
/// [`HeartbeatPorts`].
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("heartbeat")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    check_credentials(&settings).map_err(Error)?;
    let ports = ctx.port::<HeartbeatPorts>().ok_or_else(|| {
        Error(format!(
            "{}: services.heartbeat is enabled but the host provided no heartbeat ports",
            code::NO_PORTS
        ))
    })?;
    let (host, port) = settings.control_plane_endpoint()?;
    if !ports.egress.allows(&host, port) {
        return Err(Error(format!(
            "{}: add {host}:{port} to the egress allow-list for the heartbeat (services.heartbeat.control_plane)",
            code::EGRESS_NOT_ALLOWED
        )));
    }
    let clock = Arc::clone(ctx.clock());
    let service = HeartbeatService::new(ports, settings, clock)?;
    ctx.add(Arc::new(service));
    Ok(())
}
