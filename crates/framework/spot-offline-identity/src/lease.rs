//! The lease: how much time each tier has left, measured on the node's monotonic clock.
//!
//! `valid_until` is a hub-time deadline; the node turns it into a **remaining duration** when
//! it accepts a message and then counts that duration down on its monotonic clock. The wall
//! clock is never read, so setting it back or forward changes nothing. The remaining time is
//! persisted as a [`LeaseRecord`]; a restart can only restore it, never extend it:
//!
//! - same boot (the process restarted, the machine did not): the time that passed on the
//!   monotonic clock since the save is subtracted;
//! - another boot: the monotonic clock restarted and the downtime cannot be measured, so the
//!   persisted remaining time less [`MAX_CHECKPOINT_GAP`] is the most the lease can be. It is never larger than what was
//!   saved, and the next timestamp from the hub is what really extends it. Saving often (see
//!   `SnapshotStore::checkpoint`) keeps the unmeasured gap small.
//!
//! The unmeasured gap: a checkpoint is the last moment the node knew the lease. On
//! another boot the time since it is unknown, so a restore charges [`MAX_CHECKPOINT_GAP`]
//! against every tier. Only a signed hub time restores the charge, and then to the exact
//! value, never above what was saved.
//!
//! The replayed image: a party who can write the node's disk can put back an older image taken
//! right after a hub contact. Each lease record therefore names the nonce of the hub reply it
//! rests on; on another boot an image older than the newest nonce the node has ever seen
//! restores nothing (see [`Lease::restore`]). What the node cannot see is a replay of everything
//! it remembers, nonce fence included; the hub sees that at the next contact, from the node's
//! report.
//!
//! A reading that goes backwards within a boot, or a boot id that does not match, is a clock
//! fault and the lease reads as lapsed (fail closed).

use serde::{Deserialize, Serialize};

use crate::types::{CEILING_MS, NodeClock, Tier};

/// The most time, in milliseconds, that may pass between two durable checkpoints while the node
/// runs. A node that restarts on another boot cannot measure what happened since its last
/// checkpoint, so it charges this much against every tier until a signed hub time arrives
///. Callers checkpoint at least this often (every heartbeat, and on every accepted
/// signed message, which `SnapshotStore` does itself). The charge is deliberately small: it
/// bounds what the *checkpoint interval* can hide, and it cannot measure time spent powered
/// off, which only a signed hub time can.
pub const MAX_CHECKPOINT_GAP: u64 = 30_000;

/// The persisted lease: remaining milliseconds per tier at the moment of saving, and where on
/// which boot's monotonic clock that was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// Remaining milliseconds per tier at `saved_at_mono_ms`.
    pub remaining_ms: [u64; 3],
    /// The monotonic reading at the save.
    pub saved_at_mono_ms: u64,
    /// The boot the reading belongs to.
    pub boot_id: u64,
    /// The nonce of the newest hub reply this lease is based on (zero before the first). A
    /// restore on another boot refuses an image whose nonce is older than the newest the node
    /// has ever seen (see [`Lease::restore`]).
    #[serde(default)]
    pub nonce: u64,
}

/// A running lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lease {
    remaining_ms: [u64; 3],
    /// The most a signed hub time may restore each tier to: what was saved, counted down.
    /// Equal to `remaining_ms` unless a restore charged the unmeasured gap.
    restorable_ms: [u64; 3],
    anchor: NodeClock,
}

impl Lease {
    /// A lease with `remaining_ms` per tier, counting from `now`.
    pub(crate) fn start(remaining_ms: [u64; 3], now: NodeClock) -> Self {
        Self {
            remaining_ms,
            restorable_ms: remaining_ms,
            anchor: now,
        }
    }

    /// A lease with nothing left.
    pub(crate) fn lapsed(now: NodeClock) -> Self {
        Self::start([0; 3], now)
    }

    /// Restores a persisted lease at `now`. Never larger than what was saved, and never above
    /// the platform ceiling even if the stored record was altered.
    ///
    /// `nonce_floor` is the newest hub nonce the node has ever seen, as far as its backend
    /// remembers it. On another boot, an image whose nonce is older than that is a replay of an
    /// older image: it has **no remaining lease**, and the node must reach the hub (a signed
    /// timestamp or snapshot) before it continues. The same-boot path is not affected.
    pub(crate) fn restore(record: &LeaseRecord, now: NodeClock, nonce_floor: u64) -> Self {
        let mut record = *record;
        for r in &mut record.remaining_ms {
            *r = (*r).min(CEILING_MS);
        }
        let record = &record;
        if record.boot_id == now.boot_id {
            if now.mono_ms < record.saved_at_mono_ms {
                return Self::lapsed(now);
            }
            let elapsed = now.mono_ms - record.saved_at_mono_ms;
            let mut remaining = record.remaining_ms;
            for r in &mut remaining {
                *r = r.saturating_sub(elapsed);
            }
            return Self::start(remaining, now);
        }
        if record.nonce < nonce_floor {
            return Self::lapsed(now);
        }
        // Another boot: the time since the checkpoint is unknown. Charge the worst case.
        let mut lease = Self::start(record.remaining_ms, now);
        for r in &mut lease.remaining_ms {
            *r = r.saturating_sub(MAX_CHECKPOINT_GAP);
        }
        lease
    }

    /// The most a signed hub time may set `tier` to at `now`; zero on a clock fault.
    pub(crate) fn restorable(&self, tier: Tier, now: NodeClock) -> u64 {
        match self.elapsed(now) {
            Some(elapsed) => self.restorable_ms[tier.index()].saturating_sub(elapsed),
            None => 0,
        }
    }

    /// The remaining milliseconds of `tier` at `now`; zero on a clock fault.
    pub(crate) fn remaining(&self, tier: Tier, now: NodeClock) -> u64 {
        match self.elapsed(now) {
            Some(elapsed) => self.remaining_ms[tier.index()].saturating_sub(elapsed),
            None => 0,
        }
    }

    /// Whether `now` is a usable reading for this lease.
    pub(crate) fn clock_ok(&self, now: NodeClock) -> bool {
        self.elapsed(now).is_some()
    }

    /// The record to persist, as of `now`. A clock fault saves a lapsed lease.
    pub(crate) fn record(&self, now: NodeClock) -> LeaseRecord {
        let mut remaining_ms = [0; 3];
        for t in Tier::ALL {
            remaining_ms[t.index()] = self.remaining(t, now);
        }
        LeaseRecord {
            remaining_ms,
            saved_at_mono_ms: now.mono_ms,
            boot_id: now.boot_id,
            nonce: 0,
        }
    }

    fn elapsed(&self, now: NodeClock) -> Option<u64> {
        if now.boot_id != self.anchor.boot_id || now.mono_ms < self.anchor.mono_ms {
            return None;
        }
        Some(now.mono_ms - self.anchor.mono_ms)
    }
}
