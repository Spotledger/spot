//! [`Scheduler`]: one node's tick — lease, plan, hand over, then advance the mark.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use spot_core::Clock;

use crate::error::{Error, Result};
use crate::lease::{Lease, LeaseStore};
use crate::policy::{MissedRunPolicy, plan};
use crate::schedule::Schedule;
use crate::slot::{RunSlot, SlotRef};

/// The schedule-leader lease name a site's schedulers contend for.
pub fn leader_lease_name(site: &str) -> String {
    format!("scheduler:{site}")
}

/// The persisted high-water mark of one schedule: the last firing that was handed over.
///
/// Slots are planned for firings strictly after the mark, so a re-plan after a tick that died
/// between handover and mark-advance re-produces the same slots (their deduplication keys are
/// the same, the job engine drops the second enqueue) and a mark advanced too early would lose
/// runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighWater {
    /// The last handed-over firing, or `None` before the first one.
    pub last: Option<Timestamp>,
}

/// Where a site's schedule high-water marks live.
///
/// The mark must survive a restart, hence a store trait rather than node memory; the in-memory
/// implementation serves the simulation, where each simulated node owns one.
pub trait HighWaterStore: Send + Sync {
    /// The mark for `key` (`app:schedule`), or `None`.
    fn get(&self, key: &str) -> Option<HighWater>;
    /// Stores the mark for `key`.
    fn set(&self, key: &str, mark: HighWater);
}

/// A [`HighWaterStore`] in one mutex, per node in the simulation.
#[derive(Debug, Default)]
pub struct InMemoryHighWater {
    marks: std::sync::Mutex<std::collections::HashMap<String, HighWater>>,
}

impl InMemoryHighWater {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<H: HighWaterStore + ?Sized> HighWaterStore for Arc<H> {
    fn get(&self, key: &str) -> Option<HighWater> {
        (**self).get(key)
    }

    fn set(&self, key: &str, mark: HighWater) {
        (**self).set(key, mark);
    }
}

impl HighWaterStore for InMemoryHighWater {
    fn get(&self, key: &str) -> Option<HighWater> {
        self.lock().get(key).copied()
    }

    fn set(&self, key: &str, mark: HighWater) {
        self.lock().insert(key.to_owned(), mark);
    }

    // The mark is a plain Optional<Timestamp> per key; a panic elsewhere cannot leave it
    // inconsistent, so the lock needs no poison recovery beyond `into_inner`.
}

impl InMemoryHighWater {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, HighWater>> {
        self.marks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// How one tick hands a planned slot to the job engine.
///
/// The callback receives the slot and *returns* success or failure: a failure (the engine
/// refused, the enqueue rolled back) means the mark must **not** advance past that slot, so the
/// next tick re-plans it under the same deduplication key.
pub trait SlotSink: Send + Sync {
    /// Hands one slot over. `true` = accepted (the mark may advance); `false` = refused (the
    /// mark stays, the slot is re-planned next tick).
    fn deliver(&self, slot: &RunSlot) -> bool;
}

/// A sink over any `Fn(&RunSlot) -> bool`.
pub struct FnSink<F: Fn(&RunSlot) -> bool + Send + Sync> {
    f: F,
}

impl<F: Fn(&RunSlot) -> bool + Send + Sync> SlotSink for FnSink<F> {
    fn deliver(&self, slot: &RunSlot) -> bool {
        (self.f)(slot)
    }
}

/// Builds a [`SlotSink`] from a closure.
pub fn sink<F: Fn(&RunSlot) -> bool + Send + Sync + 'static>(f: F) -> Arc<dyn SlotSink> {
    Arc::new(FnSink { f })
}

/// The knobs one node's scheduler runs with.
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// The site this node schedules for: the lease name is `scheduler:<site>`.
    pub site: String,
    /// The node's own identity in the lease.
    pub node: String,
    /// The lease's time-to-live; a node quiet longer than this loses leadership.
    pub ttl: Duration,
    /// Every schedule this node may run, with the policy each applies.
    pub schedules: Vec<(Arc<Schedule>, MissedRunPolicy)>,
    /// The app the schedules belong to (their deduplication-key prefix).
    pub app: String,
}

/// One scheduler node.
///
/// The node owns its high-water store (the simulation gives every node its own, the
/// storage-backed one later is shared per site); the lease store and sink are shared between
/// the nodes of a simulation. All state transitions happen in [`Scheduler::tick`], which is
/// deliberately synchronous so the deterministic simulation drives it stepwise.
pub struct Scheduler<L: LeaseStore, H: HighWaterStore> {
    config: SchedulerConfig,
    leases: Arc<L>,
    high_water: H,
    /// The lease acquired at the last successful tick, kept to renew before re-acquiring.
    lease: Option<Lease>,
}

impl<L: LeaseStore, H: HighWaterStore> Scheduler<L, H> {
    /// A node for `config`, over the lease store and its own high-water marks.
    pub fn new(config: SchedulerConfig, leases: Arc<L>, high_water: H) -> Self {
        Self {
            config,
            leases,
            high_water,
            lease: None,
        }
    }

    /// The node's identity.
    pub fn node(&self) -> &str {
        &self.config.node
    }

    /// Renews the lease from the last tick; a lost lease (expired mid-pause) is reported and
    /// cleared so the next `tick` acquires fresh.
    pub fn renew(&mut self, clock: &dyn Clock) -> Result<Lease> {
        let Some(lease) = self.lease.take() else {
            return Err(Error::lease_lost(
                &leader_lease_name(&self.config.site),
                &self.config.node,
                0,
                "this node holds no lease",
            ));
        };
        let now = timestamp_of(clock);
        match self.leases.renew(&lease, self.config.ttl, now) {
            Ok(renewed) => {
                self.lease = Some(renewed.clone());
                Ok(renewed)
            }
            Err(err) => {
                self.lease = None;
                Err(err)
            }
        }
    }

    /// One tick at `clock`'s `now`: acquire or renew the schedule-leader lease, plan every
    /// schedule's due slots since its high-water mark, hand each slot to `sink`, and advance
    /// the mark past exactly the handed-over slots. Losing the lease mid-tick abandons the
    /// rest and returns the slots handed over so far (the error names the loss).
    ///
    /// Per ScheduleSub, the slots of one schedule are planned and handed over in scheduled
    /// order; a schedule's slots all carry the same `SlotRef` and distinct keys.
    pub fn tick(&mut self, clock: &dyn Clock, sink: &dyn SlotSink) -> Result<Vec<RunSlot>> {
        let now = timestamp_of(clock);
        let lease_name = leader_lease_name(&self.config.site);
        // Renew an existing lease (so a steady tick does not bump the token). A lease that
        // lapsed while this node was quiet is re-acquired in the same tick when nobody else
        // took it; when someone did, the acquire fails with `scheduler.lease_lost` and this
        // node's tick is over.
        let renewed = match self.lease.take() {
            Some(existing) => self.leases.renew(&existing, self.config.ttl, now).ok(),
            None => None,
        };
        let lease = match renewed {
            Some(lease) => lease,
            None => self
                .leases
                .acquire(&lease_name, &self.config.node, self.config.ttl, now)?,
        };
        self.lease = Some(lease);

        let mut handed = Vec::new();
        for (schedule, policy) in &self.config.schedules {
            let key = schedule_key(&self.config.app, &schedule.name);
            // With no mark yet this schedule has no history to recover: plan from an instant
            // just before `now`, so a firing exactly at `now` is still planned, and never from
            // the beginning of time.
            let from = self
                .high_water
                .get(&key)
                .and_then(|mark| mark.last)
                .unwrap_or(now - jiff::Span::new().nanoseconds(1));
            let slot_ref = SlotRef::from_planned(&self.config.app, schedule);
            let slots = plan(
                schedule,
                &self.config.app,
                Some(from),
                now,
                *policy,
                slot_ref,
            );
            let mut refused = false;
            for slot in &slots {
                if !sink.deliver(slot) {
                    // Stop this schedule's handover; leave the mark where it is, so the next
                    // tick re-plans this slot under the same key.
                    refused = true;
                    break;
                }
                // Advance the mark only after a successful handover (the brief's rule); the
                // mark is per schedule, so one refused slot freezes only its own schedule.
                self.advance_mark(&key, slot.scheduled_for);
                handed.push(slot.clone());
            }
            if !refused {
                // Every firing up to `now` is accounted for, handed over or dropped by the
                // policy (`skip`, the oldest of a `catch_up`): the mark moves to `now`, so the
                // next plan starts there instead of re-reading the whole window.
                self.advance_mark(&key, now);
            }
        }
        Ok(handed)
    }

    /// Moves the mark of `key` forward to `to`, never backward: a node whose clock runs behind
    /// the node that ticked before it must not re-open firings that were already handed over.
    fn advance_mark(&self, key: &str, to: Timestamp) {
        let behind = self
            .high_water
            .get(key)
            .and_then(|mark| mark.last)
            .is_none_or(|last| last < to);
        if behind {
            self.high_water.set(key, HighWater { last: Some(to) });
        }
    }

    /// The site's leader lease as this node last saw it, for the simulation's token checks.
    pub fn held_lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }
}

/// The high-water key for one schedule: `<app>:<schedule>`.
pub fn schedule_key(app: &str, schedule: &str) -> String {
    format!("{app}:{schedule}")
}

/// The clock's wall time, as the lease store's timestamps.
///
/// The `time` `OffsetDateTime` a [`Clock`] yields is converted on the second boundary: the
/// sub-second part keeps its nanoseconds, so a manual clock advanced in whole seconds round-trips
/// exactly.
pub fn timestamp_of(clock: &dyn Clock) -> Timestamp {
    let now = clock.now_utc();
    let seconds = now.unix_timestamp();
    let nanos = i32::try_from(u64::from(now.nanosecond()))
        .map_err(|_| ())
        .unwrap_or(0);
    Timestamp::new(seconds, nanos).unwrap_or(Timestamp::MIN)
}
