//! The receiving side as the loop sees it: [`Intake`], and [`IntakeApplier`] over
//! `spot_sync_node::NodeIntake`.
//!
//! A pulled batch becomes a list of [`Served`] bundles. The applier runs each through
//! `NodeIntake::receive_served` (atomic apply, parking, version-skew holds, replay idempotence
//! are all `NodeIntake`'s), counts what happened, and then persists the **serve-sequence prefix**
//! through a [`CursorStore`]. The prefix is the only thing the edge ever acknowledges: it never
//! runs ahead of the data, because it is derived from settled bundles, and the stored cursor is
//! written only after the receipts are back (the apply of each bundle is durable by then).
//!
//! The sender is untrusted. A served bundle for a tenant other than the node's own, or from a
//! source other than the configured hub, is **quarantined**: counted, never applied, never
//! counted into the prefix (so the hub re-serves and the operator sees the count).

use spot_events_contract::SourceId;
use spot_sync_node::{
    BundleApply, Delivery, NodeCatalog, NodeError, NodeIntake, ParkingStore, Receipt,
};

use crate::envelope::Served;
use crate::error::{Error, Result};

/// Where the acknowledged serve prefix is kept across restarts.
pub trait CursorStore {
    /// The highest serve sequence acknowledged so far (0 when none).
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn load(&mut self) -> Result<u64>;

    /// Stores a new prefix. Called only with a value above the stored one, after the bundles
    /// below it were applied durably.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn store(&mut self, prefix: u64) -> Result<()>;
}

/// What one batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// The acknowledged serve prefix after the batch.
    pub prefix: u64,
    /// Bundles applied (or already applied: a counted replay).
    pub applied: usize,
    /// Bundles parked until a record they reference arrives.
    pub parked: usize,
    /// Bundles held for version skew (a record this node cannot decode).
    pub held: usize,
    /// Bundles the apply path refused (nothing stored).
    pub refused: usize,
    /// Bundles quarantined: not this node's tenant or source, or an undecodable envelope.
    pub quarantined: usize,
}

impl Received {
    /// Adds another batch's counts (the prefix is the later one).
    pub fn merge(&mut self, later: Received) {
        self.prefix = self.prefix.max(later.prefix);
        self.applied += later.applied;
        self.parked += later.parked;
        self.held += later.held;
        self.refused += later.refused;
        self.quarantined += later.quarantined;
    }
}

/// The blocking receiving side, living on a [`Worker`](crate::worker::Worker) thread.
pub trait Intake {
    /// The acknowledged serve prefix from durable state.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn cursor(&mut self) -> Result<u64>;

    /// Applies the bundles of one pulled batch, in order, and persists the new prefix.
    /// `quarantined` is the number of records of the batch that did not decode (counted by the
    /// loop; they never reach here).
    ///
    /// # Errors
    ///
    /// `sync_node_host.apply_failed` when the store itself fails (not when one bundle is refused).
    fn receive(&mut self, items: Vec<Served>, quarantined: usize, now_ms: u64) -> Result<Received>;
}

/// [`Intake`] over a `NodeIntake` and a [`CursorStore`], for one tenant and one hub.
pub struct IntakeApplier<A: BundleApply, P: ParkingStore, C: NodeCatalog, S: CursorStore> {
    intake: NodeIntake<A, P, C>,
    cursor: S,
    tenant: String,
    hub_source: SourceId,
    stored: Option<u64>,
}

impl<A: BundleApply, P: ParkingStore, C: NodeCatalog, S: CursorStore> IntakeApplier<A, P, C, S> {
    /// An applier for `tenant`'s bundles from `hub_source`.
    pub fn new(
        intake: NodeIntake<A, P, C>,
        cursor: S,
        tenant: impl Into<String>,
        hub_source: SourceId,
    ) -> Self {
        Self {
            intake,
            cursor,
            tenant: tenant.into(),
            hub_source,
            stored: None,
        }
    }

    /// The wrapped intake (counters, holds, repair list), for the host's health surface.
    pub fn intake(&self) -> &NodeIntake<A, P, C> {
        &self.intake
    }

    fn stored(&mut self) -> Result<u64> {
        if let Some(value) = self.stored {
            return Ok(value);
        }
        let value = self.cursor.load()?;
        self.stored = Some(value);
        Ok(value)
    }
}

impl<A: BundleApply, P: ParkingStore, C: NodeCatalog, S: CursorStore> Intake
    for IntakeApplier<A, P, C, S>
{
    fn cursor(&mut self) -> Result<u64> {
        self.stored()
    }

    fn receive(&mut self, items: Vec<Served>, quarantined: usize, now_ms: u64) -> Result<Received> {
        let mut received = Received {
            quarantined,
            ..Received::default()
        };
        for served in items {
            if served.tenant != self.tenant {
                // Another tenant's bundle: never applied here, and not settled.
                tracing::warn!(
                    seq = served.stamp.seq,
                    "a served bundle names another tenant"
                );
                received.quarantined += 1;
                continue;
            }
            let delivery = Delivery {
                source: self.hub_source.clone(),
                tenant: served.tenant,
                bundle: served.bundle,
            };
            match self.intake.receive_served(delivery, served.stamp, now_ms) {
                Ok(Receipt::Applied(_)) => received.applied += 1,
                Ok(Receipt::Parked { .. } | Receipt::Overflow { .. }) => received.parked += 1,
                Ok(Receipt::Held(_)) => received.held += 1,
                Ok(Receipt::Refused(refusal)) => {
                    tracing::warn!(
                        seq = served.stamp.seq,
                        reason = %refusal.reason,
                        "the apply path refused a bundle"
                    );
                    received.refused += 1;
                }
                Err(NodeError::ForeignSource { .. } | NodeError::TooManyTenants { .. }) => {
                    received.quarantined += 1;
                }
                Err(error) => {
                    return Err(Error::apply_failed(format!(
                        "the intake failed at serve sequence {}: {error}",
                        served.stamp.seq
                    )));
                }
            }
        }
        self.intake.expire(now_ms);
        let stored = self.stored()?;
        let prefix = self
            .intake
            .served_prefix(&self.tenant)
            .unwrap_or(0)
            .max(stored);
        if prefix > stored {
            self.cursor.store(prefix)?;
            self.stored = Some(prefix);
        }
        received.prefix = prefix;
        Ok(received)
    }
}
