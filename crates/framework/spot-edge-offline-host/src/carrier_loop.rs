//! The carrier loop: lease requests and permission snapshot requests on the sync connection.
//!
//! The edge asks on its own turn (the hub never dials). The loop is a step function: the host
//! calls [`CarrierLoop::tick`] when the sleeper wakes it, passes the clock readings (nothing is
//! read here), and sleeps for the returned [`Tick::next_wake`].
//!
//! # The connection is a port
//!
//! [`CarrierLink`] is what the loop needs of the connection: whether the edge is connected, one
//! typed record sent, the typed records that arrived. The node loop of `spot-sync-node-host`
//! owns the real connection; until it offers a carrier seam the host cannot provide this port and
//! the module runs without a carrier (the ranges already held keep working, the readiness line
//! says so). The wire kinds are in [`crate::kinds`] (one place to rename).
//!
//! # Rules the loop keeps
//!
//! * **One in flight per kind.** One lease request at a time (series are served one after the
//!   other) and one snapshot request at a time. An unanswered request is dropped after
//!   [`CarrierSettings::answer_timeout`] and asked again with the **same id**, so a hub that did
//!   grant answers with the same range (the idempotent re-request).
//! * **Backoff on refusal and on failure.** Doubling from [`CarrierSettings::backoff_min`] to
//!   [`CarrierSettings::backoff_max`] per kind, reset by a success. A refusal code from the hub
//!   is a refusal, never an error to retry at once.
//! * **No busy loop while offline.** Not connected: nothing is sent or read, and the wake is the
//!   idle interval.
//! * **Nothing is believed unverified.** A granted range goes through [`RangeBook::install`]
//!   (strictly above the held one, for this node, durable before it is usable); a snapshot goes
//!   through the keeper (signature, rollback, fork, replay, expiry).
//! * No payload, key or number of another node is logged or returned: counts and codes only.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_offline_identity::NodeClock;

use crate::carrier::{self, LeaseAnswerMsg, SnapshotMsg};
use crate::identity::{BoxedPersistence, SnapshotKeeper};
use crate::kinds::{LEASE_ANSWER_KIND, LEASE_REQUEST_KIND, SNAPSHOT_KIND};
use crate::ranges::RangeBook;
use crate::series;

/// The stable code of a lease request that got no answer in time.
pub const LEASE_ANSWER_TIMEOUT: &str = "edge_offline.lease_answer_timeout";
/// The stable code of a snapshot request that got no answer in time.
pub const SNAPSHOT_ANSWER_TIMEOUT: &str = "edge_offline.snapshot_answer_timeout";
/// The stable code of a record of a kind an edge does not take.
pub const UNEXPECTED_RECORD: &str = "edge_offline.unexpected_record";
/// The stable code of a carrier payload that did not parse.
pub const BAD_MESSAGE: &str = "edge_offline.bad_message";

/// One typed record on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarrierRecord {
    /// The record kind ([`crate::kinds`]).
    pub kind: String,
    /// The payload ([`crate::carrier::encode`]).
    pub payload: String,
}

/// A failure of the connection, as a stable word (never a payload).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkError(pub &'static str);

/// What the loop awaits.
pub type LinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LinkError>> + Send + 'a>>;

/// The carrier's view of the sync connection.
pub trait CarrierLink: Send + Sync {
    /// Whether the edge is connected to its hub right now.
    fn connected(&self) -> bool;

    /// Sends one record on the connection's turn.
    fn send(&self, record: CarrierRecord) -> LinkFuture<'_, ()>;

    /// The records that arrived for the carrier since the last call, oldest first.
    fn receive(&self) -> LinkFuture<'_, Vec<CarrierRecord>>;
}

/// The loop's timings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarrierSettings {
    /// The first wait after a refusal or a failure.
    pub backoff_min: Duration,
    /// The longest wait.
    pub backoff_max: Duration,
    /// How long a request waits for its answer before it is asked again.
    pub answer_timeout: Duration,
    /// The wake interval when there is nothing to do or the edge is offline.
    pub idle: Duration,
}

impl Default for CarrierSettings {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(5),
            backoff_max: Duration::from_secs(900),
            answer_timeout: Duration::from_secs(60),
            idle: Duration::from_secs(30),
        }
    }
}

/// What the host reads off the clock for one tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instants {
    /// The monotonic clock.
    pub mono: Duration,
    /// The node clock the identity store takes (monotonic milliseconds and the boot id).
    pub node: NodeClock,
}

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// When to call again.
    pub next_wake: Duration,
    /// Ranges installed on this tick.
    pub ranges_installed: usize,
    /// Snapshots, timestamps or hub times accepted on this tick.
    pub snapshots_accepted: usize,
    /// Refusal or failure codes seen on this tick, in order.
    pub codes: Vec<String>,
    /// Series below the refill threshold (`SERIES_LOW`), whether or not the hub is reachable.
    pub low_series: Vec<String>,
    /// Series with nothing left (document creation in them fails with `series_exhausted`).
    pub exhausted_series: Vec<String>,
    /// Whether the edge was connected.
    pub connected: bool,
}

#[derive(Clone, Debug)]
struct InFlight {
    id: String,
    deadline: Duration,
}

#[derive(Clone, Debug, Default)]
struct Kind {
    in_flight: Option<InFlight>,
    failures: u32,
    not_before: Duration,
}

impl Kind {
    fn failed(&mut self, now: Duration, settings: &CarrierSettings) {
        let shift = self.failures.min(16);
        let delay = settings
            .backoff_min
            .saturating_mul(1_u32 << shift)
            .min(settings.backoff_max);
        self.failures = self.failures.saturating_add(1);
        self.not_before = now.saturating_add(delay);
        self.in_flight = None;
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.not_before = Duration::ZERO;
        self.in_flight = None;
    }
}

/// The keeper the loop feeds.
pub type SharedKeeper = Arc<Mutex<SnapshotKeeper<BoxedPersistence>>>;

/// The loop.
pub struct CarrierLoop {
    link: Arc<dyn CarrierLink>,
    ranges: Arc<RangeBook>,
    keeper: Option<SharedKeeper>,
    settings: CarrierSettings,
    lease: Kind,
    snapshot: Kind,
}

impl std::fmt::Debug for CarrierLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarrierLoop")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl CarrierLoop {
    /// A loop over `link`, the range book and (when the edge has an identity store) the keeper.
    pub fn new(
        link: Arc<dyn CarrierLink>,
        ranges: Arc<RangeBook>,
        keeper: Option<SharedKeeper>,
        settings: CarrierSettings,
    ) -> Self {
        Self {
            link,
            ranges,
            keeper,
            settings,
            lease: Kind::default(),
            snapshot: Kind::default(),
        }
    }

    /// One step: read what arrived, drop what timed out, send what is due.
    pub async fn tick(&mut self, now: Instants) -> Tick {
        let mut tick = Tick::default();
        if !self.link.connected() {
            // Offline is normal: nothing is sent, nothing is read, nobody spins.
            self.lease.in_flight = None;
            self.snapshot.in_flight = None;
            tick.next_wake = self.settings.idle;
            self.describe(&mut tick);
            return tick;
        }
        tick.connected = true;

        match self.link.receive().await {
            Ok(records) => {
                for record in records {
                    self.take(&record, now, &mut tick);
                }
            }
            Err(error) => {
                tick.codes.push(error.0.to_owned());
            }
        }
        self.expire(now.mono, &mut tick);
        self.send_lease(now.mono, &mut tick).await;
        self.send_snapshot(now, &mut tick).await;
        tick.next_wake = self.wake(now.mono);
        self.describe(&mut tick);
        tick
    }

    /// Fills in the state of the series as it is after this tick.
    fn describe(&self, tick: &mut Tick) {
        tick.low_series = series::low(&*self.ranges, self.ranges.series());
        tick.exhausted_series = self.ranges.exhausted();
    }

    fn take(&mut self, record: &CarrierRecord, now: Instants, tick: &mut Tick) {
        match record.kind.as_str() {
            LEASE_ANSWER_KIND => self.take_lease(&record.payload, now.mono, tick),
            SNAPSHOT_KIND => self.take_snapshot(&record.payload, now, tick),
            LEASE_REQUEST_KIND => {
                // A request kind arriving at an edge is not ours to answer.
                tick.codes.push(UNEXPECTED_RECORD.to_owned());
            }
            _ => {}
        }
    }

    fn take_lease(&mut self, payload: &str, now: Duration, tick: &mut Tick) {
        let Ok(answer) = carrier::decode::<LeaseAnswerMsg>(payload) else {
            tick.codes.push(BAD_MESSAGE.to_owned());
            return;
        };
        let Some(waiting) = &self.lease.in_flight else {
            return;
        };
        if waiting.id != answer.request_id() {
            // An answer to a request this loop is no longer waiting on: stale, ignored.
            return;
        }
        match &answer {
            LeaseAnswerMsg::Refused { code, .. } => {
                tick.codes.push(code.clone());
                self.lease.failed(now, &self.settings);
            }
            LeaseAnswerMsg::Granted { .. } => match self.ranges.install(&answer) {
                Ok(_) => {
                    tick.ranges_installed += 1;
                    self.lease.succeeded();
                }
                Err(error) => {
                    tick.codes.push(error.code.to_owned());
                    self.lease.failed(now, &self.settings);
                }
            },
        }
    }

    fn take_snapshot(&mut self, payload: &str, now: Instants, tick: &mut Tick) {
        let Ok(message) = carrier::decode::<SnapshotMsg>(payload) else {
            tick.codes.push(BAD_MESSAGE.to_owned());
            return;
        };
        let Some(waiting) = &self.snapshot.in_flight else {
            return;
        };
        if waiting.id != message.request_id() {
            return;
        }
        match message {
            SnapshotMsg::Refused { code, .. } => {
                tick.codes.push(code);
                self.snapshot.failed(now.mono, &self.settings);
            }
            SnapshotMsg::Request { .. } => {
                tick.codes.push(UNEXPECTED_RECORD.to_owned());
            }
            SnapshotMsg::Issued {
                message, companion, ..
            } => {
                let Some(keeper) = &self.keeper else { return };
                let outcome = {
                    let mut keeper = keeper.lock().unwrap_or_else(|p| p.into_inner());
                    let first = keeper.accept(&message, now.node);
                    match (first, &companion) {
                        // The paired hub time is accepted right after the snapshot; its own
                        // refusal does not undo the snapshot, but is reported.
                        (Ok(accepted), Some(time)) => {
                            keeper.accept(time, now.node).map(|_| accepted)
                        }
                        (other, _) => other,
                    }
                };
                match outcome {
                    Ok(_) => {
                        tick.snapshots_accepted += 1;
                        self.snapshot.succeeded();
                    }
                    Err(error) => {
                        tick.codes.push(error.code().to_owned());
                        self.snapshot.failed(now.mono, &self.settings);
                    }
                }
            }
        }
    }

    fn expire(&mut self, now: Duration, tick: &mut Tick) {
        if self
            .lease
            .in_flight
            .as_ref()
            .is_some_and(|f| now >= f.deadline)
        {
            tick.codes.push(LEASE_ANSWER_TIMEOUT.to_owned());
            self.lease.failed(now, &self.settings);
        }
        if self
            .snapshot
            .in_flight
            .as_ref()
            .is_some_and(|f| now >= f.deadline)
        {
            tick.codes.push(SNAPSHOT_ANSWER_TIMEOUT.to_owned());
            self.snapshot.failed(now, &self.settings);
        }
    }

    async fn send_lease(&mut self, now: Duration, tick: &mut Tick) {
        if self.lease.in_flight.is_some() || now < self.lease.not_before {
            return;
        }
        let due = series::due(&*self.ranges, self.ranges.series(), true);
        let Some(request) = due.first().and_then(|name| self.ranges.request_for(name)) else {
            return;
        };
        let Ok(payload) = carrier::encode(&request) else {
            return;
        };
        let record = CarrierRecord {
            kind: LEASE_REQUEST_KIND.to_owned(),
            payload,
        };
        match self.link.send(record).await {
            Ok(()) => {
                self.lease.in_flight = Some(InFlight {
                    id: request.request_id,
                    deadline: now.saturating_add(self.settings.answer_timeout),
                });
            }
            Err(error) => {
                tick.codes.push(error.0.to_owned());
                self.lease.failed(now, &self.settings);
            }
        }
    }

    async fn send_snapshot(&mut self, now: Instants, tick: &mut Tick) {
        let Some(keeper) = &self.keeper else { return };
        if self.snapshot.in_flight.is_some() || now.mono < self.snapshot.not_before {
            return;
        }
        let request = {
            let guard = keeper.lock().unwrap_or_else(|p| p.into_inner());
            if !guard.refresh_due(now.node) {
                return;
            }
            guard.request(guard.next_request_id())
        };
        let Some(request) = request else { return };
        let Ok(payload) = carrier::encode(&request) else {
            return;
        };
        let record = CarrierRecord {
            kind: SNAPSHOT_KIND.to_owned(),
            payload,
        };
        match self.link.send(record).await {
            Ok(()) => {
                self.snapshot.in_flight = Some(InFlight {
                    id: request.request_id().to_owned(),
                    deadline: now.mono.saturating_add(self.settings.answer_timeout),
                });
            }
            Err(error) => {
                tick.codes.push(error.0.to_owned());
                self.snapshot.failed(now.mono, &self.settings);
            }
        }
    }

    /// The next wake: the nearest answer deadline or backoff end that matters, else idle.
    fn wake(&self, now: Duration) -> Duration {
        let mut wake = self.settings.idle;
        for kind in [&self.lease, &self.snapshot] {
            if let Some(flight) = &kind.in_flight {
                wake = wake.min(flight.deadline.saturating_sub(now));
            } else if kind.not_before > now {
                wake = wake.min(kind.not_before - now);
            }
        }
        wake.max(Duration::from_millis(100))
    }

    /// Consecutive failures of the lease request (for tests and the readiness line).
    pub fn lease_failures(&self) -> u32 {
        self.lease.failures
    }

    /// Consecutive failures of the snapshot request.
    pub fn snapshot_failures(&self) -> u32 {
        self.snapshot.failures
    }
}
