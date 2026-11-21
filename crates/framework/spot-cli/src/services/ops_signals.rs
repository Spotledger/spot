//! The signals service (WP-OPS-01.T3): polls the journal, sync, quarantine, conditions and
//! maintenance-gate status the hosted subsystems already expose and sets the gauges of
//! `spot-ops-signals` on a fixed interval, so an operator's Prometheus sees what the console sees.
//!
//! Gauges are state, not events: they are answered by asking the subsystem. Event counters
//! (`spot_journal_ship_batches_total` and the like) are recorded where the event happens, by the
//! module that owns it, with the `spot-ops-signals` helpers; this service never invents them.
//!
//! # What is published
//!
//! A source that is **absent** on a site (no journal service, no sync hub, no quarantine) is not
//! asked, and its series are never set: absent from `/metrics` is the honest answer, a zero would
//! read as "healthy". A field a source cannot give (the status of the journal has no oldest
//! unshipped time, so `spot_journal_ship_lag_seconds` is not set here; the sync health view has
//! no send queue, obligations or holds) stays absent the same way.
//!
//! # A source that fails
//!
//! Every source is read concurrently with its own timeout. A source that fails or times out keeps
//! its **previous** value for at most three intervals; the fourth failing interval drops the
//! site's reading. The `metrics` facade (0.24) cannot remove a gauge, so a series with no reading
//! from any site is set to the **sentinel `-1`** ("unknown"; no real value is negative), which
//! `docs/running-spot/signals.md` documents. Failures are logged once per transition
//! (readable to failing, failing to readable), never per tick, and never with an error text.
//!
//! # Two sites in one process
//!
//! The recorder is process-wide and the series carry no site label by design, so each site
//! publishes its reading to a [`Board`] and the series show the aggregate: **counts are summed,
//! ages are the maximum, a closed gate on any site reads 1**, and the lag statistics are the
//! maximum over sites. Per-site detail belongs to the admin screens, not to Prometheus.
//!
//! The service is [`Level::Optional`]: it never stops a site. It adds no HTTP route.
//! Settings: `[services.ops_signals]` (`enabled`, `interval_seconds`, `timeout_seconds`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use spot_core::Clock;
use spot_mgmt_journal::results::AuditOutcome;
use spot_mgmt_journal::{JournalOps, SnapshotListArgs};
use spot_mgmt_sync::SyncOps;
use spot_migration_apply::gate::AdmissionGate;
use spot_ops_signals as signals;
use spot_ops_signals::catalogue::SERIES;
use spot_ops_signals::conditions::OpenCounts;
use spot_ops_signals::sync::{NodeCounts, NodeLag};
use spot_sync_health::{HealthView, NodeStatus};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::services::conditions::ConditionsHandle;
use crate::services::quarantine::{Item, QuarantineBackend, State};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name.
pub const NAME: &str = "ops_signals";
/// After every module whose ports it reads (quarantine is 1400).
pub const ORDER: u32 = 1900;
/// A failing source keeps its last value for this many intervals, then is dropped.
pub const MAX_STALE_INTERVALS: u32 = 3;
/// What a series with no reading from any site is set to: "unknown". No real value is negative.
pub const UNKNOWN: f64 = -1.0;

/// `[services.ops_signals]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Whether the service runs (it also needs `services.metrics.enabled`).
    pub enabled: bool,
    /// Seconds between collections, 5 to 300.
    pub interval_seconds: u64,
    /// Seconds a source may take, 1 to the interval.
    pub timeout_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: 15,
            timeout_seconds: 5,
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// A message naming the setting.
    pub fn validate(&self) -> Result<()> {
        if !(5..=300).contains(&self.interval_seconds) {
            return Err(Error(
                "services.ops_signals.interval_seconds must be between 5 and 300".to_owned(),
            ));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > self.interval_seconds {
            return Err(Error(
                "services.ops_signals.timeout_seconds must be at least 1 and not above the interval"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

// ---- readings ----

/// Which subsystem a reading is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// The journal.
    Journal,
    /// Sync.
    Sync,
    /// Quarantine.
    Quarantine,
    /// Conditions.
    Conditions,
    /// The maintenance gate.
    Gate,
}

impl Kind {
    /// The name logs use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Journal => "journal",
            Kind::Sync => "sync",
            Kind::Quarantine => "quarantine",
            Kind::Conditions => "conditions",
            Kind::Gate => "gate",
        }
    }

    /// The gauges of this kind (the ones this service sets).
    pub fn gauges(self) -> &'static [&'static str] {
        match self {
            Kind::Journal => &[
                signals::journal::UNSHIPPED_ENTRIES,
                signals::journal::SNAPSHOT_AGE_SECONDS,
                signals::journal::AUDIT_AGE_SECONDS,
            ],
            Kind::Sync => &[signals::sync::NODES, signals::sync::NODE_LAG_SECONDS],
            Kind::Quarantine => &[
                signals::quarantine::OPEN,
                signals::quarantine::OLDEST_AGE_SECONDS,
            ],
            Kind::Conditions => &[signals::conditions::OPEN],
            Kind::Gate => &[signals::migration::GATE_CLOSED],
        }
    }
}

/// What the journal's status says, as numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalReading {
    /// Entries between the local head and the off-site position; `None` when it cannot be known.
    pub unshipped_entries: Option<u64>,
    /// Age of the newest snapshot.
    pub snapshot_age: Option<Duration>,
    /// Time since the last clean audit.
    pub audit_age: Option<Duration>,
}

/// What the sync health view says, as numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReading {
    /// Nodes per state.
    pub nodes: NodeCounts,
    /// The lag over the nodes that have an estimate; `None` when none has.
    pub lag: Option<NodeLag>,
}

/// What quarantine holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuarantineReading {
    /// Open items.
    pub open: u64,
    /// Age of the oldest open item.
    pub oldest_age: Duration,
}

/// One source's reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// The journal.
    Journal(JournalReading),
    /// Sync.
    Sync(SyncReading),
    /// Quarantine.
    Quarantine(QuarantineReading),
    /// Open conditions by severity.
    Conditions(OpenCounts),
    /// Whether the gate is closed.
    Gate(bool),
}

impl Reading {
    /// Which subsystem the reading is about.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Reading::Journal(_) => Kind::Journal,
            Reading::Sync(_) => Kind::Sync,
            Reading::Quarantine(_) => Kind::Quarantine,
            Reading::Conditions(_) => Kind::Conditions,
            Reading::Gate(_) => Kind::Gate,
        }
    }
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

fn max_opt<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Folds the readings of several sites into one: counts summed, ages maximal, a closed gate
/// anywhere closed. `None` for an empty list.
#[must_use]
pub fn aggregate(readings: &[Reading]) -> Option<Reading> {
    let mut folded: Option<Reading> = None;
    for next in readings {
        folded = Some(match (folded, next) {
            (None, next) => next.clone(),
            (Some(Reading::Journal(a)), Reading::Journal(b)) => Reading::Journal(JournalReading {
                unshipped_entries: add_opt(a.unshipped_entries, b.unshipped_entries),
                snapshot_age: max_opt(a.snapshot_age, b.snapshot_age),
                audit_age: max_opt(a.audit_age, b.audit_age),
            }),
            (Some(Reading::Sync(a)), Reading::Sync(b)) => Reading::Sync(SyncReading {
                nodes: NodeCounts {
                    healthy: a.nodes.healthy.saturating_add(b.nodes.healthy),
                    lagging: a.nodes.lagging.saturating_add(b.nodes.lagging),
                    silent: a.nodes.silent.saturating_add(b.nodes.silent),
                    suspended: a.nodes.suspended.saturating_add(b.nodes.suspended),
                    too_old: a.nodes.too_old.saturating_add(b.nodes.too_old),
                },
                lag: match (a.lag, b.lag) {
                    (Some(x), Some(y)) => Some(NodeLag {
                        p50: x.p50.max(y.p50),
                        p95: x.p95.max(y.p95),
                        max: x.max.max(y.max),
                    }),
                    (x, y) => x.or(y),
                },
            }),
            (Some(Reading::Quarantine(a)), Reading::Quarantine(b)) => {
                Reading::Quarantine(QuarantineReading {
                    open: a.open.saturating_add(b.open),
                    oldest_age: a.oldest_age.max(b.oldest_age),
                })
            }
            (Some(Reading::Conditions(a)), Reading::Conditions(b)) => {
                Reading::Conditions(OpenCounts {
                    info: a.info.saturating_add(b.info),
                    amber: a.amber.saturating_add(b.amber),
                    red: a.red.saturating_add(b.red),
                })
            }
            (Some(Reading::Gate(a)), Reading::Gate(b)) => Reading::Gate(a || *b),
            // Readings of different kinds are never mixed: the board groups by kind.
            (Some(other), _) => other,
        });
    }
    folded
}

/// Sets the series of `reading`. A field that is `None` is left alone.
fn publish(reading: &Reading) {
    match reading {
        Reading::Journal(j) => {
            if let Some(n) = j.unshipped_entries {
                signals::journal::set_unshipped_entries(n);
            }
            if let Some(age) = j.snapshot_age {
                signals::journal::set_snapshot_age(age);
            }
            if let Some(age) = j.audit_age {
                signals::journal::set_audit_age(age);
            }
        }
        Reading::Sync(s) => {
            signals::sync::set_nodes(&s.nodes);
            if let Some(lag) = &s.lag {
                signals::sync::set_node_lag(lag);
            }
        }
        Reading::Quarantine(q) => {
            signals::quarantine::set_open(q.open);
            signals::quarantine::set_oldest_age(q.oldest_age);
        }
        Reading::Conditions(c) => signals::conditions::set_open(c),
        Reading::Gate(closed) => signals::migration::set_gate_closed(*closed),
    }
}

/// Sets every gauge of `kind` to [`UNKNOWN`].
fn mark_unknown(kind: Kind) {
    for name in kind.gauges() {
        let Some(spec) = SERIES.iter().find(|s| s.name == *name) else {
            continue;
        };
        match spec.labels.first() {
            None => metrics::gauge!(spec.name).set(UNKNOWN),
            Some(label) => {
                for value in label.values {
                    metrics::gauge!(spec.name, label.name => *value).set(UNKNOWN);
                }
            }
        }
    }
}

/// The readings of every site in the process, and what the series show of them.
#[derive(Default)]
pub struct Board {
    readings: Mutex<BTreeMap<(Kind, String), Reading>>,
    published: Mutex<BTreeSet<Kind>>,
}

impl Board {
    /// An empty board.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide board the hosted service uses.
    pub fn global() -> Arc<Board> {
        static BOARD: OnceLock<Arc<Board>> = OnceLock::new();
        Arc::clone(BOARD.get_or_init(|| Arc::new(Board::new())))
    }

    /// Records `site`'s reading and publishes the aggregate of its kind.
    pub fn set(&self, site: &str, reading: Reading) {
        let kind = reading.kind();
        self.readings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((kind, site.to_owned()), reading);
        self.publish(kind);
    }

    /// Drops `site`'s reading of `kind` and publishes what is left (the sentinel when nothing).
    pub fn remove(&self, site: &str, kind: Kind) {
        self.readings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(kind, site.to_owned()));
        self.publish(kind);
    }

    /// The aggregate of `kind` over the sites, if any site has a reading.
    #[must_use]
    pub fn aggregate_of(&self, kind: Kind) -> Option<Reading> {
        let all: Vec<Reading> = self
            .readings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|(_, r)| r.clone())
            .collect();
        aggregate(&all)
    }

    fn publish(&self, kind: Kind) {
        let mut published = self.published.lock().unwrap_or_else(|p| p.into_inner());
        match self.aggregate_of(kind) {
            Some(reading) => {
                publish(&reading);
                published.insert(kind);
            }
            None => {
                // Nothing left to show: a series that was shown goes to the sentinel; one that
                // never was stays absent.
                if published.remove(&kind) {
                    mark_unknown(kind);
                }
            }
        }
    }
}

// ---- the mappings (pure) ----

/// `rfc3339` text to the age it has at `now`, zero when it lies in the future.
fn age_of(text: &str, now: OffsetDateTime) -> Option<Duration> {
    let at = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    let secs = (now - at).whole_seconds();
    Some(Duration::from_secs(u64::try_from(secs).unwrap_or(0)))
}

/// The journal: the entry gap between the local head and the off-site position (same generation
/// only), the newest snapshot's age and the age of the last audit **when it was clean**.
#[must_use]
pub fn map_journal(
    status: &spot_mgmt_journal::results::StatusResult,
    snapshots: &spot_mgmt_journal::results::SnapshotListResult,
    audit: &spot_mgmt_journal::results::AuditResult,
    now: OffsetDateTime,
) -> JournalReading {
    use spot_mgmt_journal::results::JournalState;
    let unshipped_entries = match (&status.state, status.head, status.shipped) {
        (JournalState::NoTarget, _, _) => None,
        (_, Some(head), Some(shipped)) if head.generation == shipped.generation => {
            Some(head.seq.saturating_sub(shipped.seq))
        }
        (_, Some(head), None) => Some(head.seq),
        (_, None, _) => Some(0),
        _ => None,
    };
    let snapshot_age = snapshots
        .snapshots
        .first()
        .and_then(|s| age_of(&s.taken_at, now));
    let audit_age = match (audit.outcome, audit.ran_at.as_deref()) {
        (AuditOutcome::Match, Some(at)) => age_of(at, now),
        _ => None,
    };
    JournalReading {
        unshipped_entries,
        snapshot_age,
        audit_age,
    }
}

/// Sync: nodes per state and the lag over nodes with an estimate. The health view lists at most
/// `MAX_ROWS` nodes, worst first; when it is cut, the nodes not shown are counted healthy (they
/// are the least bad).
#[must_use]
pub fn map_sync(view: &HealthView) -> SyncReading {
    let mut nodes = NodeCounts::default();
    let mut lags: Vec<u64> = Vec::new();
    for row in &view.nodes {
        match row.status {
            NodeStatus::Connected => nodes.healthy += 1,
            NodeStatus::Lagging => nodes.lagging += 1,
            NodeStatus::Silent => nodes.silent += 1,
            NodeStatus::Suspended => nodes.suspended += 1,
            NodeStatus::TooOld => nodes.too_old += 1,
        }
        if let Some(lag) = row.lag_estimate {
            lags.push(lag.as_u64() / 1000);
        }
    }
    let shown = u64::try_from(view.nodes.len()).unwrap_or(u64::MAX);
    let total = u64::try_from(view.counts.nodes).unwrap_or(u64::MAX);
    if view.truncated && total > shown {
        nodes.healthy = nodes.healthy.saturating_add(total - shown);
    }
    lags.sort_unstable();
    let rank = |percent: usize| -> Duration {
        let n = lags.len();
        let index = (n * percent).div_ceil(100).saturating_sub(1).min(n - 1);
        Duration::from_secs(lags[index])
    };
    let lag = if lags.is_empty() {
        None
    } else {
        Some(NodeLag {
            p50: rank(50),
            p95: rank(95),
            max: Duration::from_secs(lags[lags.len() - 1]),
        })
    };
    SyncReading { nodes, lag }
}

/// Quarantine: items not yet closed or discarded, and the age of the oldest.
#[must_use]
pub fn map_quarantine(items: &[Item]) -> QuarantineReading {
    let open: Vec<&Item> = items
        .iter()
        .filter(|i| matches!(i.state, State::Parked | State::Held | State::Retrying))
        .collect();
    let oldest = open
        .iter()
        .map(|i| u64::try_from(i.age_secs).unwrap_or(0))
        .max()
        .unwrap_or(0);
    QuarantineReading {
        open: u64::try_from(open.len()).unwrap_or(u64::MAX),
        oldest_age: Duration::from_secs(oldest),
    }
}

/// Conditions: open ones per severity (every severity is set, zeros included).
#[must_use]
pub fn map_conditions(open: &[spot_conditions::Open]) -> OpenCounts {
    let mut counts = OpenCounts::default();
    for condition in open {
        match condition.severity {
            spot_conditions::Severity::Info => counts.info += 1,
            spot_conditions::Severity::Amber => counts.amber += 1,
            spot_conditions::Severity::Red => counts.red += 1,
        }
    }
    counts
}

// ---- sources ----

/// Something the collector can ask.
#[async_trait]
pub trait Source: Send + Sync {
    /// The subsystem it reads.
    fn kind(&self) -> Kind;
    /// One reading at `now`. The error is never logged or published: it only means "failed".
    async fn read(&self, now: OffsetDateTime) -> std::result::Result<Reading, String>;
}

struct JournalSource(Arc<dyn JournalOps>);
#[async_trait]
impl Source for JournalSource {
    fn kind(&self) -> Kind {
        Kind::Journal
    }
    async fn read(&self, now: OffsetDateTime) -> std::result::Result<Reading, String> {
        let status = self.0.status().await.map_err(|e| e.reason)?;
        let snapshots = self
            .0
            .snapshot_list(SnapshotListArgs { limit: 1 })
            .await
            .map_err(|e| e.reason)?;
        let audit = self.0.audit_last().await.map_err(|e| e.reason)?;
        Ok(Reading::Journal(map_journal(
            &status, &snapshots, &audit, now,
        )))
    }
}

struct SyncSource(Arc<dyn SyncOps>);
#[async_trait]
impl Source for SyncSource {
    fn kind(&self) -> Kind {
        Kind::Sync
    }
    async fn read(&self, _now: OffsetDateTime) -> std::result::Result<Reading, String> {
        let view = self.0.health().await.map_err(|e| e.reason)?;
        Ok(Reading::Sync(map_sync(&view)))
    }
}

struct QuarantineSource {
    backend: Arc<dyn QuarantineBackend>,
    tenant: String,
}
#[async_trait]
impl Source for QuarantineSource {
    fn kind(&self) -> Kind {
        Kind::Quarantine
    }
    async fn read(&self, _now: OffsetDateTime) -> std::result::Result<Reading, String> {
        let items = self
            .backend
            .summary(&self.tenant)
            .await
            .map_err(|_| "unavailable".to_owned())?;
        Ok(Reading::Quarantine(map_quarantine(&items)))
    }
}

struct ConditionsSource(Arc<ConditionsHandle>);
#[async_trait]
impl Source for ConditionsSource {
    fn kind(&self) -> Kind {
        Kind::Conditions
    }
    async fn read(&self, _now: OffsetDateTime) -> std::result::Result<Reading, String> {
        let open = self.0.open().await.map_err(|_| "unavailable".to_owned())?;
        Ok(Reading::Conditions(map_conditions(&open)))
    }
}

struct GateSource(Arc<dyn AdmissionGate>);
#[async_trait]
impl Source for GateSource {
    fn kind(&self) -> Kind {
        Kind::Gate
    }
    async fn read(&self, _now: OffsetDateTime) -> std::result::Result<Reading, String> {
        Ok(Reading::Gate(!self.0.status().is_open()))
    }
}

// ---- the collector ----

#[derive(Default)]
struct Track {
    failures: u32,
    dropped: bool,
    last_ok: Option<OffsetDateTime>,
}

/// One transition of a source, kept so a test can see the log is once per transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    /// A source that was readable failed.
    Failing(Kind),
    /// A failing source answered again.
    Readable(Kind),
    /// A source failed for more than [`MAX_STALE_INTERVALS`] intervals and its reading was dropped.
    Dropped(Kind),
}

/// Reads every source each interval and publishes through the board.
pub struct Collector {
    site: String,
    sources: Vec<Arc<dyn Source>>,
    board: Arc<Board>,
    clock: Arc<dyn Clock>,
    timeout: Duration,
    tracks: Mutex<HashMap<Kind, Track>>,
    transitions: Mutex<Vec<Transition>>,
}

impl Collector {
    /// A collector for `site`'s `sources`.
    pub fn new(
        site: impl Into<String>,
        sources: Vec<Arc<dyn Source>>,
        board: Arc<Board>,
        clock: Arc<dyn Clock>,
        timeout: Duration,
    ) -> Self {
        Self {
            site: site.into(),
            sources,
            board,
            clock,
            timeout,
            tracks: Mutex::new(HashMap::new()),
            transitions: Mutex::new(Vec::new()),
        }
    }

    /// How many sources this site has.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// The transitions so far, oldest first.
    #[must_use]
    pub fn transitions(&self) -> Vec<Transition> {
        self.transitions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// `ops_signals: <n> sources, last ok <age>`.
    #[must_use]
    pub fn status_line(&self) -> String {
        let now = self.clock.now_utc();
        let last = self
            .tracks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter_map(|t| t.last_ok)
            .max();
        let age = match last {
            Some(at) => format!("{}s ago", (now - at).whole_seconds().max(0)),
            None => "never".to_owned(),
        };
        format!("ops_signals: {} sources, last ok {age}", self.sources.len())
    }

    /// Reads every source once, concurrently, each under the timeout.
    pub async fn collect_once(&self) {
        let now = self.clock.now_utc();
        let mut set = JoinSet::new();
        for source in &self.sources {
            let source = Arc::clone(source);
            let limit = self.timeout;
            set.spawn(async move {
                let kind = source.kind();
                let outcome = match tokio::time::timeout(limit, source.read(now)).await {
                    Ok(Ok(reading)) => Some(reading),
                    _ => None,
                };
                (kind, outcome)
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((kind, outcome)) = joined {
                self.settle(kind, outcome, now);
            }
        }
    }

    fn note(&self, transition: Transition) {
        self.transitions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(transition);
    }

    fn settle(&self, kind: Kind, outcome: Option<Reading>, now: OffsetDateTime) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|p| p.into_inner());
        let track = tracks.entry(kind).or_default();
        match outcome {
            Some(reading) => {
                if track.failures > 0 {
                    tracing::info!(site = %self.site, source = kind.as_str(), "ops_signals: source readable again");
                    self.note(Transition::Readable(kind));
                }
                track.failures = 0;
                track.dropped = false;
                track.last_ok = Some(now);
                drop(tracks);
                self.board.set(&self.site, reading);
            }
            None => {
                track.failures = track.failures.saturating_add(1);
                if track.failures == 1 {
                    tracing::warn!(site = %self.site, source = kind.as_str(), "ops_signals: source failing; keeping its last value");
                    self.note(Transition::Failing(kind));
                }
                if track.failures > MAX_STALE_INTERVALS && !track.dropped {
                    track.dropped = true;
                    drop(tracks);
                    tracing::warn!(site = %self.site, source = kind.as_str(), "ops_signals: source failing for too long; its series are dropped");
                    self.note(Transition::Dropped(kind));
                    self.board.remove(&self.site, kind);
                }
            }
        }
    }

    /// Withdraws this site's readings (the service stopped).
    pub fn withdraw(&self) {
        for source in &self.sources {
            self.board.remove(&self.site, source.kind());
        }
    }
}

// ---- the service ----

struct OpsSignals {
    collector: Arc<Collector>,
    interval: Duration,
}

impl Service for OpsSignals {
    fn name(&self) -> &'static str {
        NAME
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
            tracing::info!("{}", self.collector.status_line());
            loop {
                self.collector.collect_once().await;
                tokio::select! {
                    () = tokio::time::sleep(self.interval) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.collector.withdraw();
            Ok(())
        })
    }
}

/// Registers the service when `[services.ops_signals]` and `[services.metrics]` allow it. The
/// ports are looked up in a finaliser, after every module registered; a source whose port is
/// absent is not asked.
///
/// # Errors
///
/// A setting out of range, or a context without a site.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let settings: Settings = ctx.settings_or_default(NAME)?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let env = *ctx.env()?;
    if !env.loaded.common.services.metrics.enabled {
        return Ok(());
    }
    let site = env.site.site.clone();
    ctx.finally(move |ctx| {
        let mut sources: Vec<Arc<dyn Source>> = Vec::new();
        if let Some(ops) = ctx.port::<dyn JournalOps>() {
            sources.push(Arc::new(JournalSource(ops)));
        }
        if let Some(ops) = ctx.port::<dyn SyncOps>() {
            sources.push(Arc::new(SyncSource(ops)));
        }
        if let Some(backend) = ctx.port::<dyn QuarantineBackend>() {
            sources.push(Arc::new(QuarantineSource {
                backend,
                tenant: site.clone(),
            }));
        }
        if let Some(handle) = ctx.port::<ConditionsHandle>() {
            sources.push(Arc::new(ConditionsSource(handle)));
        }
        if let Some(gate) = ctx.port::<dyn AdmissionGate>() {
            sources.push(Arc::new(GateSource(gate)));
        }
        // The recorder is installed once per process by `main`; this registers the descriptions.
        if let Err(e) = signals::describe_all() {
            tracing::warn!(error = %e, "ops_signals: the series could not be described");
        }
        let collector = Arc::new(Collector::new(
            site,
            sources,
            Board::global(),
            Arc::clone(ctx.clock()),
            Duration::from_secs(settings.timeout_seconds),
        ));
        ctx.add(Arc::new(OpsSignals {
            collector,
            interval: Duration::from_secs(settings.interval_seconds),
        }));
        Ok(())
    });
    Ok(())
}
