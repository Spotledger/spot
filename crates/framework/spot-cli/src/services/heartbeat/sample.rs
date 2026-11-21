//! The samplers of the heartbeat emitter (WP-CTL-03.T3): one small object per fleet-health signal
//! the site can honestly measure, and the collector that runs them.
//!
//! # The closed set
//!
//! [`Signal`] mirrors `spot_fleet_health::Signal` name for name and value range for value range
//! (the control plane accepts nothing else: `POST /cp/v1/host/heartbeat` refuses an unknown name
//! or an out-of-range value). Every value is an integer; there is no float and no free label. A
//! sampler returns a number in the signal's unit and nothing else, so no user name, document name
//! or path can ride along.
//!
//! # Never on the request path
//!
//! A sampler is called only by the emitter's own task, through `spawn_blocking` and a time budget.
//! The samplers shipped here read atomics and cheap host facts through narrow traits; none takes a
//! lock a request takes, so sampling cannot delay or fail a write.
//!
//! # Silence means "host down"
//!
//! [`collect`] always returns a `health_probe`. When its sampler fails (or every sampler fails)
//! the value is `0` and the failing samplers' names are left out of the batch: the control plane
//! then sees a red host that is still talking, which is a different thing from a host that has
//! stopped sending.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::services::{Health, Level, State};

/// A signal's value in basis points at most.
pub const BASIS_POINTS: i64 = 10_000;
/// The licence state codes the control plane accepts (`spot_fleet_health::LicenceState`).
pub const LICENCE_ACTIVE: i64 = 0;
/// Grace period.
pub const LICENCE_GRACE: i64 = 1;
/// Expired.
pub const LICENCE_EXPIRED: i64 = 2;

/// The closed set of fleet-health signals, spelled as the control plane spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// 1 when every required service is healthy, else 0.
    HealthProbe,
    /// Failed requests in the last window, basis points.
    ErrorRate,
    /// Age in seconds of the oldest waiting job.
    QueueLag,
    /// Failed jobs in the window.
    JobFailures,
    /// Replication delay in seconds (replica role only).
    ReplicationLag,
    /// Disk in use, basis points.
    DiskUsage,
    /// Seconds until a certificate expires; negative once expired.
    CertificateExpiry,
    /// The licence state code.
    LicenceState,
}

impl Signal {
    /// Every signal.
    pub const ALL: [Signal; 8] = [
        Self::HealthProbe,
        Self::ErrorRate,
        Self::QueueLag,
        Self::JobFailures,
        Self::ReplicationLag,
        Self::DiskUsage,
        Self::CertificateExpiry,
        Self::LicenceState,
    ];

    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HealthProbe => "health_probe",
            Self::ErrorRate => "error_rate",
            Self::QueueLag => "queue_lag",
            Self::JobFailures => "job_failures",
            Self::ReplicationLag => "replication_lag",
            Self::DiskUsage => "disk_usage",
            Self::CertificateExpiry => "certificate_expiry",
            Self::LicenceState => "licence_state",
        }
    }

    /// Whether `value` is in the signal's domain (the control plane refuses the batch otherwise).
    #[must_use]
    pub const fn accepts(self, value: i64) -> bool {
        match self {
            Self::HealthProbe => value == 0 || value == 1,
            Self::ErrorRate | Self::DiskUsage => value >= 0 && value <= BASIS_POINTS,
            Self::LicenceState => value >= LICENCE_ACTIVE && value <= LICENCE_EXPIRED,
            Self::CertificateExpiry => true,
            Self::QueueLag | Self::JobFailures | Self::ReplicationLag => value >= 0,
        }
    }
}

/// Whom an observation is about: the tenant (this site) or the node (this host).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The tenant.
    Tenant,
    /// The calling host.
    Node,
}

/// One measured value, before it has a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The signal.
    pub signal: Signal,
    /// Whom it is about.
    pub scope: Scope,
    /// The value, in the signal's unit.
    pub value: i64,
}

/// One signal's sampler. Called only by the emitter's own task.
pub trait SignalSampler: Send + Sync {
    /// The signal this sampler measures.
    fn signal(&self) -> Signal;
    /// Whom the value is about.
    fn scope(&self) -> Scope;
    /// The value now.
    ///
    /// # Errors
    /// A fixed sentence (never a value, name or path) when the signal cannot be measured now.
    fn sample(&self) -> Result<i64, String>;
}

/// What [`collect`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    /// The readings that were measured, `health_probe` always among them.
    pub readings: Vec<Reading>,
    /// How many samplers failed or ran over budget (their names are deliberately not kept).
    pub failed: usize,
}

/// Runs every sampler, each within `budget`, and returns what they measured.
///
/// A sampler that errors, panics, answers outside its signal's domain or runs over `budget` is
/// omitted. `health_probe` is always present: its sampler's value, or `0` when that sampler is
/// missing or failed, so silence from this host can only mean the host is down.
pub async fn collect(samplers: &[Arc<dyn SignalSampler>], budget: Duration) -> Collected {
    let mut readings = Vec::with_capacity(samplers.len().saturating_add(1));
    let mut failed = 0_usize;
    let mut have_probe = false;
    for sampler in samplers {
        let signal = sampler.signal();
        let scope = sampler.scope();
        let one = Arc::clone(sampler);
        let run = tokio::task::spawn_blocking(move || one.sample());
        let value = match tokio::time::timeout(budget, run).await {
            Ok(Ok(Ok(value))) if signal.accepts(value) => Some(value),
            _ => None,
        };
        match value {
            Some(value) => {
                have_probe |= signal == Signal::HealthProbe;
                readings.push(Reading {
                    signal,
                    scope,
                    value,
                });
            }
            None => failed = failed.saturating_add(1),
        }
    }
    if !have_probe {
        readings.push(Reading {
            signal: Signal::HealthProbe,
            scope: Scope::Node,
            value: 0,
        });
    }
    // One reading per (signal, scope): a duplicate sampler would double a series.
    readings.sort_by_key(|r| (r.signal, r.scope));
    readings.dedup_by_key(|r| (r.signal, r.scope));
    Collected { readings, failed }
}

// ---- samplers ------------------------------------------------------------------------------

/// `health_probe`: 1 when every required service is running, else 0 (the site's own aggregate).
pub struct HealthProbeSampler {
    health: Health,
}

impl HealthProbeSampler {
    /// A sampler over the site's service health.
    #[must_use]
    pub fn new(health: Health) -> Self {
        Self { health }
    }
}

impl SignalSampler for HealthProbeSampler {
    fn signal(&self) -> Signal {
        Signal::HealthProbe
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn sample(&self) -> Result<i64, String> {
        let states = self.health.states();
        let ok = states
            .values()
            .all(|(level, state)| *level != Level::Required || *state == State::Running);
        Ok(i64::from(ok))
    }
}

/// The request counters the host already keeps. Plain atomics: reading them takes no lock.
#[derive(Debug, Default)]
pub struct RequestCounters {
    /// Requests answered.
    pub total: AtomicU64,
    /// Requests that failed on the server side (5xx).
    pub failed: AtomicU64,
}

/// `error_rate`: failed over total since the previous sample, in basis points. No requests in the
/// window is a rate of 0.
pub struct ErrorRateSampler {
    counters: Arc<RequestCounters>,
    last: Mutex<(u64, u64)>,
}

impl ErrorRateSampler {
    /// A sampler over `counters`. The first sample covers everything since the counters began.
    #[must_use]
    pub fn new(counters: Arc<RequestCounters>) -> Self {
        Self {
            counters,
            last: Mutex::new((0, 0)),
        }
    }
}

/// Basis points of `failed` in `total`, integer only, never above 10,000.
#[must_use]
pub fn basis_points(failed: u64, total: u64) -> i64 {
    if total == 0 {
        return 0;
    }
    let bp = u128::from(failed.min(total)) * 10_000 / u128::from(total);
    i64::try_from(bp).unwrap_or(BASIS_POINTS)
}

impl SignalSampler for ErrorRateSampler {
    fn signal(&self) -> Signal {
        Signal::ErrorRate
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn sample(&self) -> Result<i64, String> {
        let total = self.counters.total.load(Ordering::Relaxed);
        let failed = self.counters.failed.load(Ordering::Relaxed);
        let mut last = self
            .last
            .lock()
            .map_err(|_| "the error-rate window is unavailable".to_owned())?;
        // A counter that went backwards (a restart) starts a new window.
        let (dt, df) = if total >= last.0 && failed >= last.1 {
            (total - last.0, failed - last.1)
        } else {
            (total, failed)
        };
        *last = (total, failed);
        Ok(basis_points(df, dt))
    }
}

/// The job store's own numbers, read by one cheap query each.
pub trait JobStats: Send + Sync {
    /// Age in seconds of the oldest job waiting to run (0 when none waits).
    ///
    /// # Errors
    /// A fixed sentence.
    fn oldest_waiting_secs(&self) -> Result<i64, String>;
    /// Jobs that failed in the last window.
    ///
    /// # Errors
    /// A fixed sentence.
    fn failures_in_window(&self) -> Result<i64, String>;
}

/// `queue_lag` from the job store.
pub struct QueueLagSampler(pub Arc<dyn JobStats>);

impl SignalSampler for QueueLagSampler {
    fn signal(&self) -> Signal {
        Signal::QueueLag
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn sample(&self) -> Result<i64, String> {
        self.0.oldest_waiting_secs()
    }
}

/// `job_failures` from the job store.
pub struct JobFailuresSampler(pub Arc<dyn JobStats>);

impl SignalSampler for JobFailuresSampler {
    fn signal(&self) -> Signal {
        Signal::JobFailures
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn sample(&self) -> Result<i64, String> {
        self.0.failures_in_window()
    }
}

/// Capacity of the filesystem a path is on. The host implements it with `statvfs`.
pub trait DiskProbe: Send + Sync {
    /// `(total bytes, free bytes)` of the filesystem holding `path`.
    ///
    /// # Errors
    /// A fixed sentence.
    fn capacity(&self, path: &std::path::Path) -> Result<(u64, u64), String>;
}

/// `disk_usage` of the data directory's filesystem, in basis points.
pub struct DiskUsageSampler {
    probe: Arc<dyn DiskProbe>,
    path: PathBuf,
}

impl DiskUsageSampler {
    /// A sampler for the filesystem of `path` (the data directory).
    #[must_use]
    pub fn new(probe: Arc<dyn DiskProbe>, path: PathBuf) -> Self {
        Self { probe, path }
    }
}

impl SignalSampler for DiskUsageSampler {
    fn signal(&self) -> Signal {
        Signal::DiskUsage
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn sample(&self) -> Result<i64, String> {
        let (total, free) = self.probe.capacity(&self.path)?;
        if total == 0 {
            return Err("the data filesystem reports no capacity".to_owned());
        }
        Ok(basis_points(total.saturating_sub(free), total))
    }
}

/// The `notAfter` of a certificate, Unix seconds. The host implements it by parsing the PEM it
/// loaded; the emitter never sees a key.
pub trait CertificateSource: Send + Sync {
    /// The expiry instant.
    ///
    /// # Errors
    /// A fixed sentence.
    fn not_after(&self) -> Result<i64, String>;
}

/// `certificate_expiry`: seconds until the soonest of the certificates it watches expires
/// (negative once one has). The host's mutual-TLS certificate always, the site's server
/// certificate when it has one.
pub struct CertificateExpirySampler {
    sources: Vec<Arc<dyn CertificateSource>>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl CertificateExpirySampler {
    /// A sampler over `sources`, reading the time from `now` (Unix seconds; the host's clock).
    #[must_use]
    pub fn new(
        sources: Vec<Arc<dyn CertificateSource>>,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self { sources, now }
    }
}

impl SignalSampler for CertificateExpirySampler {
    fn signal(&self) -> Signal {
        Signal::CertificateExpiry
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn sample(&self) -> Result<i64, String> {
        let mut soonest: Option<i64> = None;
        for source in &self.sources {
            let at = source.not_after()?;
            soonest = Some(soonest.map_or(at, |s| s.min(at)));
        }
        let at = soonest.ok_or_else(|| "no certificate to watch".to_owned())?;
        Ok(at.saturating_sub((self.now)()))
    }
}

/// The licence state, from the licence port when WP-CTL-04.T3 provides one.
pub trait LicenceReader: Send + Sync {
    /// 0 active, 1 grace, 2 expired.
    ///
    /// # Errors
    /// A fixed sentence.
    fn state_code(&self) -> Result<i64, String>;
}

/// `licence_state`. Only built when a licence port exists: with none the signal is left out and
/// no value is invented.
pub struct LicenceStateSampler(pub Arc<dyn LicenceReader>);

impl SignalSampler for LicenceStateSampler {
    fn signal(&self) -> Signal {
        Signal::LicenceState
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn sample(&self) -> Result<i64, String> {
        self.0.state_code()
    }
}

/// How far a replica is behind its primary, seconds.
pub trait ReplicaLag: Send + Sync {
    /// The lag now.
    ///
    /// # Errors
    /// A fixed sentence.
    fn lag_secs(&self) -> Result<i64, String>;
}

/// `replication_lag`. Only built on a replica role.
pub struct ReplicationLagSampler(pub Arc<dyn ReplicaLag>);

impl SignalSampler for ReplicationLagSampler {
    fn signal(&self) -> Signal {
        Signal::ReplicationLag
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn sample(&self) -> Result<i64, String> {
        self.0.lag_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basis_points_is_integer_and_bounded() {
        assert_eq!(basis_points(0, 0), 0);
        assert_eq!(basis_points(1, 3), 3333);
        assert_eq!(basis_points(5, 2), 10_000);
        assert_eq!(basis_points(u64::MAX, u64::MAX), 10_000);
    }

    #[test]
    fn domains_match_the_control_plane() {
        assert!(Signal::HealthProbe.accepts(1));
        assert!(!Signal::HealthProbe.accepts(2));
        assert!(!Signal::ErrorRate.accepts(10_001));
        assert!(Signal::CertificateExpiry.accepts(-5));
        assert!(!Signal::LicenceState.accepts(3));
        assert_eq!(Signal::ALL.len(), 8);
    }
}
