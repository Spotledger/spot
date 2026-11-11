//! The node side: [`SnapshotStore`] accepts, persists and counts down the signed permission
//! snapshot (ADR-070 rule 12.13 b and c).
//!
//! Rules, in the order they are checked (a rejection changes nothing, not even a counter):
//!
//! 1. the key delegation is signed by the trusted root, names this tenant, does not go back in
//!    version and delegates two different keys;
//! 2. the signature verifies under the delegated key;
//! 3. the message is for this tenant and this node;
//! 4. every tier's lease (deadline minus signing time) lies inside the platform floor and
//!    ceiling;
//! 5. a lower epoch is a rollback, an equal epoch with other content is a fork, the same
//!    snapshot again is a replay;
//! 6. a message whose deadlines have all passed on the hub's clock is expired;
//! 7. the new state is persisted **before** it replaces the old one.
//!
//! Hub time enters the store only inside a message the hub signed: the `issued_at` of a
//! snapshot, the `signed_at` of a timestamp, or a [`SignedHubTime`]. Nothing a
//! heartbeat or a transport says about time is believed, and the node's own wall clock is never
//! read. The hub clock only moves forward: a signed time that is not later than the newest one
//! accepted is refused and changes nothing. A lease can never exceed its message's own
//! deadline minus the later of the signing time and the newest signed hub time.
//!
//! Every timestamp and hub time carries a **fresh hub nonce**, strictly higher than the last;
//! the node remembers the newest in its sealed state, in the lease record. The node also tells
//! the hub what it holds ([`SnapshotStore::report`], [`SnapshotStore::node_request`]), so a
//! node that ran on a replayed disk image is seen at its next contact. On another boot a
//! lease image older than the newest nonce the node's backend has ever seen restores no time
//! ([`crate::lease`]).
//!
//! After a restart on another boot the lease is charged [`crate::lease::MAX_CHECKPOINT_GAP`]
//! for the time since the last checkpoint that nobody measured; a [`SignedHubTime`]
//! (or a snapshot or timestamp) is what restores it.

use serde::{Deserialize, Serialize};
use spot_bundle_signing::PublicKey;

use crate::error::Rejection;
use crate::lease::{Lease, LeaseRecord};
use crate::node::{CertLifecycle, CertRecord};
use crate::signing::{
    Message, NodeReport, NodeRequest, RequestKind, SignedDelegation, SignedHubTime, SignedSnapshot,
    SignedTimestamp, verify_delegation, verify_hub_time, verify_snapshot, verify_timestamp,
};
use crate::snapshot::{Snapshot, SnapshotHash, TierDeadlines};
use crate::types::{CEILING_MS, FLOOR_MS, HubMs, NodeClock, NodeId, TenantId, Tier};

/// What the node trusts from the start: its own identity and the offline root key.
#[derive(Clone, Debug)]
pub struct TrustAnchor {
    /// The tenant.
    pub tenant: TenantId,
    /// This node.
    pub node: NodeId,
    /// The root public key.
    pub root: PublicKey,
}

/// A message as it arrived. It carries the node's own clocks only: hub time is read from the
/// signed message, never from the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// The node's clocks when the message arrived.
    pub clock: NodeClock,
}

/// What was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptedKind {
    /// A new snapshot.
    Snapshot,
    /// A timestamp that extended the lease of the held snapshot.
    Timestamp,
    /// A signed hub time: the clock moved forward and a charged lease was restored.
    HubTime,
}

/// The result of an accepted message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// Which kind.
    pub kind: AcceptedKind,
    /// The epoch now held.
    pub epoch: u64,
    /// The lease per tier now running, in milliseconds.
    pub remaining_ms: [u64; 3],
}

/// What the store persists. The signed snapshot is kept whole so a restart can verify it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persisted {
    /// The snapshot in force, as received.
    pub signed: Option<SignedSnapshot>,
    /// The highest epoch ever accepted.
    pub highest_epoch: u64,
    /// The hash of the snapshot at `highest_epoch`.
    pub snapshot_hash: Option<SnapshotHash>,
    /// The highest delegation version accepted.
    pub delegation_version: u64,
    /// The hub clock: the newest signed time accepted (snapshot issue time, timestamp time or
    /// signed hub time). It never goes backwards.
    pub last_attested: HubMs,
    /// The deadlines in force (a timestamp may have moved them).
    pub deadlines: Option<TierDeadlines>,
    /// The lease.
    pub lease: LeaseRecord,
    /// The node certificate's remaining life.
    pub certificate: Option<CertRecord>,
}

/// A persistence failure. It carries no detail on purpose: the cause is in the backend's own log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistError;

/// Where the node keeps its state across restarts. `save` must be durable when it returns.
pub trait Persistence {
    /// The saved state, if any.
    ///
    /// # Errors
    /// [`PersistError`] when the backend cannot read.
    fn load(&self) -> Result<Option<Persisted>, PersistError>;
    /// Saves the state, replacing the previous one.
    ///
    /// # Errors
    /// [`PersistError`] when the state is not durably stored.
    fn save(&mut self, state: &Persisted) -> Result<(), PersistError>;
    /// The newest hub nonce this backend has ever seen, kept apart from the state image where
    /// the backend can (the sealed backend keeps it in the fence slot). The store reads it after
    /// `load`: a lease image on another boot whose nonce is older than this restores no time.
    /// Zero when the backend keeps no such memory.
    fn nonce_floor(&self) -> u64 {
        0
    }
}

/// An in-memory persistence, for tests and single-process use.
#[derive(Clone, Debug, Default)]
pub struct MemoryPersistence {
    /// The saved state.
    pub state: Option<Persisted>,
    /// When set, every save fails.
    pub fail_saves: bool,
    /// The newest hub nonce seen, kept apart from `state` (as a fence slot would). It rises on
    /// every save and on `load`; a test may set it to model a fence that survived an image
    /// being put back.
    pub seen_nonce: u64,
}

impl Persistence for MemoryPersistence {
    fn load(&self) -> Result<Option<Persisted>, PersistError> {
        Ok(self.state.clone())
    }

    fn nonce_floor(&self) -> u64 {
        self.seen_nonce
    }

    fn save(&mut self, state: &Persisted) -> Result<(), PersistError> {
        if self.fail_saves {
            return Err(PersistError);
        }
        self.seen_nonce = self.seen_nonce.max(state.lease.nonce);
        self.state = Some(state.clone());
        Ok(())
    }
}

/// The node's snapshot store.
pub struct SnapshotStore<P: Persistence> {
    pub(crate) anchor: TrustAnchor,
    persistence: P,
    pub(crate) signed: Option<SignedSnapshot>,
    highest_epoch: u64,
    snapshot_hash: Option<SnapshotHash>,
    delegation_version: u64,
    last_attested: HubMs,
    hub_nonce: u64,
    deadlines: Option<TierDeadlines>,
    pub(crate) lease: Lease,
    pub(crate) cert: CertLifecycle,
    pub(crate) overrides: crate::authorise::Overrides,
    recovered_from_damage: bool,
}

impl<P: Persistence> SnapshotStore<P> {
    /// Opens the store at `now`, restoring what was persisted.
    ///
    /// Anything that cannot be restored fails closed: an unreadable record, or a stored
    /// snapshot that no longer verifies, leaves the store with **no snapshot** (every
    /// authorisation denies) but keeps the epoch counters it could read, so an old snapshot
    /// cannot be replayed into it.
    #[must_use]
    pub fn open(anchor: TrustAnchor, persistence: P, now: NodeClock) -> Self {
        let mut store = Self {
            anchor,
            persistence,
            signed: None,
            highest_epoch: 0,
            snapshot_hash: None,
            delegation_version: 0,
            last_attested: 0,
            hub_nonce: 0,
            deadlines: None,
            lease: Lease::lapsed(now),
            cert: CertLifecycle::none(now),
            overrides: crate::authorise::Overrides::default(),
            recovered_from_damage: false,
        };
        match store.persistence.load() {
            Ok(Some(saved)) => store.restore(saved, now),
            Ok(None) => {}
            Err(PersistError) => store.recovered_from_damage = true,
        }
        store
    }

    fn restore(&mut self, saved: Persisted, now: NodeClock) {
        self.highest_epoch = saved.highest_epoch;
        self.snapshot_hash = saved.snapshot_hash;
        self.delegation_version = saved.delegation_version;
        self.last_attested = saved.last_attested;
        self.hub_nonce = saved.lease.nonce;
        let nonce_floor = self.persistence.nonce_floor();
        self.cert = CertLifecycle::restore(saved.certificate.as_ref(), now);
        let intact = saved.signed.as_ref().is_some_and(|s| {
            self.verify_stored(s).is_ok()
                && Some(s.snapshot.hash()) == saved.snapshot_hash
                && s.snapshot.epoch == saved.highest_epoch
        });
        if intact {
            self.signed = saved.signed;
            self.deadlines = saved.deadlines;
            self.lease = Lease::restore(&saved.lease, now, nonce_floor);
        } else {
            self.recovered_from_damage = saved.signed.is_some();
            self.lease = Lease::lapsed(now);
        }
    }

    /// Whether the saved state was unreadable or failed verification at `open`.
    #[must_use]
    pub fn recovered_from_damage(&self) -> bool {
        self.recovered_from_damage
    }

    /// The snapshot in force, if any.
    #[must_use]
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.signed.as_ref().map(|s| &s.snapshot)
    }

    /// The highest epoch ever accepted (zero before the first).
    #[must_use]
    pub fn highest_epoch(&self) -> u64 {
        self.highest_epoch
    }

    /// The nonce of the newest hub reply accepted (zero before the first).
    #[must_use]
    pub fn hub_nonce(&self) -> u64 {
        self.hub_nonce
    }

    /// What the node holds, for the hub. It is built from the node's own state, so a node that
    /// ran on a replayed image reports the replayed values.
    #[must_use]
    pub fn report(&self) -> NodeReport {
        NodeReport {
            highest_epoch: self.highest_epoch,
            snapshot_hash: self.snapshot_hash,
            last_attested: self.last_attested,
            hub_nonce: self.hub_nonce,
        }
    }

    /// The node's request to the hub for signed hub time or a snapshot, with its
    /// [`NodeReport`]. Sign it with [`crate::signing::sign_node_request`]; the hub refuses a
    /// request without the report.
    #[must_use]
    pub fn node_request(&self, kind: RequestKind) -> NodeRequest {
        NodeRequest {
            tenant: self.anchor.tenant.clone(),
            node: self.anchor.node.clone(),
            kind,
            report: Some(self.report()),
        }
    }

    /// The deadlines in force.
    #[must_use]
    pub fn deadlines(&self) -> Option<TierDeadlines> {
        self.deadlines
    }

    /// The hub clock: the newest hub time the hub signed and the node accepted (zero before the
    /// first). It only moves forward.
    #[must_use]
    pub fn hub_time_ms(&self) -> HubMs {
        self.last_attested
    }

    /// The remaining milliseconds of `tier`'s lease at `now` (zero when lapsed or on a clock fault).
    #[must_use]
    pub fn lease_remaining_ms(&self, tier: Tier, now: NodeClock) -> u64 {
        self.lease.remaining(tier, now)
    }

    /// Gives the persistence back (a test or an owner that restarts the store).
    #[must_use]
    pub fn into_persistence(self) -> P {
        self.persistence
    }

    /// Saves the remaining lease as of `now`. Call it at least every
    /// [`crate::lease::MAX_CHECKPOINT_GAP`] (every heartbeat): a restart on another boot charges
    /// that much against the lease, so a longer interval would hide time the charge does not cover.
    /// Every accepted signed message checkpoints by itself.
    ///
    /// # Errors
    /// [`Rejection::PersistFailed`] when the state is not durably stored.
    pub fn checkpoint(&mut self, now: NodeClock) -> Result<(), Rejection> {
        let state = self.persisted(now);
        self.persistence
            .save(&state)
            .map_err(|PersistError| Rejection::PersistFailed)
    }

    fn persisted(&self, now: NodeClock) -> Persisted {
        Persisted {
            signed: self.signed.clone(),
            highest_epoch: self.highest_epoch,
            snapshot_hash: self.snapshot_hash,
            delegation_version: self.delegation_version,
            last_attested: self.last_attested,
            deadlines: self.deadlines,
            lease: self.lease_record(now),
            certificate: self.cert.record(now),
        }
    }

    fn lease_record(&self, now: NodeClock) -> LeaseRecord {
        LeaseRecord {
            nonce: self.hub_nonce,
            ..self.lease.record(now)
        }
    }

    /// Accepts a snapshot, a timestamp or a signed hub time, or says exactly why not. The store is unchanged on
    /// a rejection.
    ///
    /// # Errors
    /// A [`Rejection`] naming the first rule that failed.
    pub fn accept(&mut self, message: &Message, rx: Received) -> Result<Accepted, Rejection> {
        match message {
            Message::Snapshot(signed) => self.accept_snapshot(signed, rx),
            Message::Timestamp(signed) => self.accept_timestamp(signed, rx),
            Message::HubTime(signed) => self.accept_hub_time(signed, rx),
        }
    }

    fn check_delegation(&self, delegation: &SignedDelegation) -> Result<(), Rejection> {
        verify_delegation(&self.anchor.root, &self.anchor.tenant, delegation)?;
        let body = &delegation.body;
        if body.version < self.delegation_version {
            return Err(Rejection::DelegationRollback {
                got: body.version,
                held: self.delegation_version,
            });
        }
        if body.snapshot_key == body.timestamp_key {
            return Err(Rejection::BadDelegation);
        }
        Ok(())
    }

    fn verify_signed_snapshot(&self, signed: &SignedSnapshot) -> Result<(), Rejection> {
        self.check_delegation(&signed.delegation)?;
        self.verify_stored(signed)
    }

    /// Signature and identity, without the delegation-version rule: a stored snapshot may be
    /// older than a delegation a later timestamp raised.
    fn verify_stored(&self, signed: &SignedSnapshot) -> Result<(), Rejection> {
        verify_delegation(&self.anchor.root, &self.anchor.tenant, &signed.delegation)?;
        verify_snapshot(signed)?;
        self.check_identity(&signed.snapshot)
    }

    fn check_identity(&self, snapshot: &Snapshot) -> Result<(), Rejection> {
        if snapshot.tenant != self.anchor.tenant {
            return Err(Rejection::WrongTenant);
        }
        if snapshot.node != self.anchor.node {
            return Err(Rejection::WrongNode);
        }
        Ok(())
    }

    fn accept_snapshot(
        &mut self,
        signed: &SignedSnapshot,
        rx: Received,
    ) -> Result<Accepted, Rejection> {
        self.verify_signed_snapshot(signed)?;
        let snapshot = &signed.snapshot;
        snapshot.validate()?;
        check_leases(&snapshot.valid_until, snapshot.issued_at)?;
        let hash = snapshot.hash();
        if snapshot.epoch < self.highest_epoch {
            return Err(Rejection::Rollback {
                got: snapshot.epoch,
                held: self.highest_epoch,
            });
        }
        if snapshot.epoch == self.highest_epoch {
            return if self.snapshot_hash == Some(hash) {
                Err(Rejection::Replay {
                    epoch: snapshot.epoch,
                })
            } else {
                Err(Rejection::Fork {
                    epoch: snapshot.epoch,
                })
            };
        }
        let remaining = remaining_from(
            &snapshot.valid_until,
            snapshot.issued_at,
            self.last_attested,
        )?;
        let next = Next {
            signed: Some(signed.clone()),
            highest_epoch: snapshot.epoch,
            snapshot_hash: Some(hash),
            delegation_version: signed.delegation.body.version,
            last_attested: snapshot.issued_at.max(self.last_attested),
            hub_nonce: self.hub_nonce,
            deadlines: Some(snapshot.valid_until),
            remaining,
        };
        self.commit(next, rx.clock, AcceptedKind::Snapshot)
    }

    fn accept_timestamp(
        &mut self,
        signed: &SignedTimestamp,
        rx: Received,
    ) -> Result<Accepted, Rejection> {
        self.check_delegation(&signed.delegation)?;
        verify_timestamp(signed)?;
        let ts = &signed.timestamp;
        if ts.tenant != self.anchor.tenant {
            return Err(Rejection::WrongTenant);
        }
        if ts.node != self.anchor.node {
            return Err(Rejection::WrongNode);
        }
        let held = self.signed.as_ref().ok_or(Rejection::TimestampMismatch)?;
        if ts.epoch != self.highest_epoch || self.snapshot_hash != Some(ts.snapshot_hash) {
            return Err(Rejection::TimestampMismatch);
        }
        if ts.signed_at <= self.last_attested || ts.nonce <= self.hub_nonce {
            return Err(Rejection::StaleTimestamp);
        }
        check_leases(&ts.valid_until, ts.signed_at)?;
        let remaining = remaining_from(&ts.valid_until, ts.signed_at, self.last_attested)?;
        let next = Next {
            signed: Some(held.clone()),
            highest_epoch: self.highest_epoch,
            snapshot_hash: self.snapshot_hash,
            delegation_version: signed.delegation.body.version.max(self.delegation_version),
            last_attested: ts.signed_at,
            hub_nonce: ts.nonce,
            deadlines: Some(ts.valid_until),
            remaining,
        };
        self.commit(next, rx.clock, AcceptedKind::Timestamp)
    }

    /// A signed hub time: moves the hub clock forward and restores a lease a restart charged.
    /// It carries no deadlines, so it can never lengthen a lease past what was saved or past
    /// the held deadlines minus the signed time.
    fn accept_hub_time(
        &mut self,
        signed: &SignedHubTime,
        rx: Received,
    ) -> Result<Accepted, Rejection> {
        self.check_delegation(&signed.delegation)?;
        verify_hub_time(signed)?;
        let time = &signed.time;
        if time.tenant != self.anchor.tenant {
            return Err(Rejection::WrongTenant);
        }
        if time.node != self.anchor.node {
            return Err(Rejection::WrongNode);
        }
        let held = self.signed.as_ref().ok_or(Rejection::TimestampMismatch)?;
        let deadlines = self.deadlines.ok_or(Rejection::TimestampMismatch)?;
        if time.epoch < self.highest_epoch {
            return Err(Rejection::TimestampMismatch);
        }
        // A rollback or a replay: the clock only moves forward.
        if time.hub_ms <= self.last_attested || time.nonce <= self.hub_nonce {
            return Err(Rejection::StaleTimestamp);
        }
        let mut remaining = [0; 3];
        for tier in Tier::ALL {
            remaining[tier.index()] = deadlines
                .get(tier)
                .saturating_sub(time.hub_ms)
                .min(self.lease.restorable(tier, rx.clock));
        }
        let next = Next {
            signed: Some(held.clone()),
            highest_epoch: self.highest_epoch,
            snapshot_hash: self.snapshot_hash,
            delegation_version: signed.delegation.body.version.max(self.delegation_version),
            last_attested: time.hub_ms,
            hub_nonce: time.nonce,
            deadlines: Some(deadlines),
            remaining,
        };
        self.commit(next, rx.clock, AcceptedKind::HubTime)
    }

    /// Persists `next`, and only if that worked makes it the state in force.
    fn commit(
        &mut self,
        next: Next,
        clock: NodeClock,
        kind: AcceptedKind,
    ) -> Result<Accepted, Rejection> {
        let lease = Lease::start(next.remaining, clock);
        let record = LeaseRecord {
            nonce: next.hub_nonce,
            ..lease.record(clock)
        };
        let state = Persisted {
            signed: next.signed.clone(),
            highest_epoch: next.highest_epoch,
            snapshot_hash: next.snapshot_hash,
            delegation_version: next.delegation_version,
            last_attested: next.last_attested,
            deadlines: next.deadlines,
            lease: record,
            certificate: self.cert.record(clock),
        };
        self.persistence
            .save(&state)
            .map_err(|PersistError| Rejection::PersistFailed)?;
        self.signed = next.signed;
        self.highest_epoch = next.highest_epoch;
        self.snapshot_hash = next.snapshot_hash;
        self.delegation_version = next.delegation_version;
        self.last_attested = next.last_attested;
        self.hub_nonce = next.hub_nonce;
        self.deadlines = next.deadlines;
        self.lease = lease;
        self.recovered_from_damage = false;
        Ok(Accepted {
            kind,
            epoch: next.highest_epoch,
            remaining_ms: next.remaining,
        })
    }
}

struct Next {
    signed: Option<SignedSnapshot>,
    highest_epoch: u64,
    snapshot_hash: Option<SnapshotHash>,
    delegation_version: u64,
    last_attested: HubMs,
    hub_nonce: u64,
    deadlines: Option<TierDeadlines>,
    remaining: [u64; 3],
}

/// Every tier's lease (deadline minus signing time) must lie in `[floor, ceiling]`: the hub
/// clamps tenant settings into that range, so anything else is not from a correct hub.
fn check_leases(deadlines: &TierDeadlines, signed_at: HubMs) -> Result<(), Rejection> {
    for tier in Tier::ALL {
        let lease = deadlines.get(tier).saturating_sub(signed_at);
        if !(FLOOR_MS..=CEILING_MS).contains(&lease) {
            return Err(Rejection::BoundsOutOfRange { tier });
        }
    }
    Ok(())
}

/// The lease left per tier when the hub clock reads `hub_clock_ms`. The count starts at the
/// later of the signing time and the newest signed hub time, so a message signed earlier than
/// the clock cannot stretch the lease past `deadline - hub_clock_ms`.
fn remaining_from(
    deadlines: &TierDeadlines,
    signed_at: HubMs,
    hub_clock_ms: HubMs,
) -> Result<[u64; 3], Rejection> {
    let base = signed_at.max(hub_clock_ms);
    let mut remaining = [0; 3];
    for tier in Tier::ALL {
        remaining[tier.index()] = deadlines.get(tier).saturating_sub(base);
    }
    if remaining.iter().all(|r| *r == 0) {
        return Err(Rejection::Expired);
    }
    Ok(remaining)
}
