//! The durable intake (WP-RTB-02.T12): [`DurableIntake`] is [`Intake`] over a `NodeIntake` that
//! was **restored** from durable state and keeps that state current.
//!
//! [`IntakeApplier`](crate::applier::IntakeApplier) acknowledges the serve prefix of a fresh
//! `NodeIntake`; after a restart that intake knows nothing of the bundles parked in the store, of
//! the bundles on the repair list, of the skew holds or of the serve prefix. This type closes that:
//!
//! * [`DurableIntake::restore`] is the only way to build one. It loads the saved state
//!   ([`StateStore::load`], the stored serve cursor), seeds the intake with
//!   `NodeIntake::restore` and only then is ready. A restore that fails is an error the factory
//!   returns: the worker answers every call with it, so nothing is acknowledged from a node that
//!   could not restore (`sync_node_intake.not_restored`).
//! * After each receive the skew holds and the repair list are persisted, **before** the serve
//!   cursor moves. The cursor is therefore never above what the stores can rebuild: a crash at any
//!   point leaves a cursor at or below the true prefix, and the hub re-serves the rest.
//!
//! # Order of one receive
//!
//! 1. every bundle through `NodeIntake::receive_served` (each apply is durable by itself);
//! 2. `expire` (parked bundles past their age go to the repair list);
//! 3. holds saved when they changed;
//! 4. repair items stored (given back to the intake when the store fails, none is lost);
//! 5. the serve cursor stored.
//!
//! The crash points ([`Failpoints`]) sit between these steps so a test can stop the node at each.
//! They are `None` in production.
//!
//! No message carries a record, a key or a value: names, counts and positions only.

use std::sync::Arc;
use std::time::Duration;

use spot_events_contract::SourceId;
use spot_pg_bridge::Bridge;
use spot_sync_node::{
    BundleApply, Delivery, HoldSet, NodeCatalog, NodeError, NodeIntake, ParkingStore, Receipt,
    RepairItem, RestoreState,
};

use crate::applier::{CursorStore, Intake, Received};
use crate::envelope::Served;
use crate::error::{Error, Result};

/// Stable code: the intake was asked to acknowledge before (or without) a completed restore.
pub const NOT_RESTORED: &str = "sync_node_intake.not_restored";
/// Stable code: a restore was refused (the node crate's code, carried through).
pub const RESTORE_REFUSED: &str = spot_sync_node::RESTORE_REFUSED;

/// Every `sync_node_intake.*` code this crate raises.
pub const CODES: [&str; 2] = [NOT_RESTORED, RESTORE_REFUSED];

/// Where the intake's restore state lives besides the serve cursor and the ledger.
pub trait StateStore {
    /// What a restart needs for `tenant`: the applied journal prefix, the saved skew holds, the
    /// first position of every parked bundle and the repair list. `served_prefix` is filled by
    /// the caller from the cursor store, whatever this returns for it.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` when the state cannot be read whole: a partial answer would
    /// let the node acknowledge more than it applied, so it never returns one.
    fn load(&mut self, tenant: &str) -> Result<RestoreState>;

    /// Saves the tenant's skew holds.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`.
    fn save_holds(&mut self, tenant: &str, holds: &HoldSet) -> Result<()>;

    /// Stores repair items, in order, in one transaction.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed`; nothing is stored then.
    fn push_repair(&mut self, items: &[RepairItem]) -> Result<()>;
}

/// A point of a receive where a test may stop the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// After the bundle at this index of the batch was received (applied, parked, held or
    /// refused), before the next one.
    AfterBundle(usize),
    /// After every bundle and the expiry sweep, before anything is persisted.
    AfterReceive,
    /// After the holds were saved, before the repair list.
    AfterHolds,
    /// After the repair items were stored, before the serve cursor.
    AfterRepair,
}

/// A crash simulator: when it answers `true` for a point, the call stops there as a process death
/// would, and the intake refuses every later call.
pub trait Failpoints: Send + Sync {
    /// Whether to stop at `point`.
    fn crash_at(&self, point: Point) -> bool;
}

/// [`Intake`] over a restored `NodeIntake`, a [`CursorStore`] and a [`StateStore`].
pub struct DurableIntake<
    A: BundleApply,
    P: ParkingStore,
    C: NodeCatalog,
    S: CursorStore,
    T: StateStore,
> {
    intake: NodeIntake<A, P, C>,
    cursor: S,
    store: T,
    tenant: String,
    hub_source: SourceId,
    stored: u64,
    saved_holds: HoldSet,
    dead: bool,
    failpoints: Option<Arc<dyn Failpoints>>,
}

impl<A: BundleApply, P: ParkingStore, C: NodeCatalog, S: CursorStore, T: StateStore>
    DurableIntake<A, P, C, S, T>
{
    /// Restores `intake` for `tenant` from `store` and `cursor`, and returns the ready intake.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` when the saved state cannot be read whole;
    /// `sync_node_intake.restore_refused` when the intake refuses the state.
    pub fn restore(
        mut intake: NodeIntake<A, P, C>,
        mut cursor: S,
        mut store: T,
        tenant: impl Into<String>,
        hub_source: SourceId,
    ) -> Result<Self> {
        let tenant = tenant.into();
        let mut state = store.load(&tenant)?;
        let stored = cursor.load()?;
        state.served_prefix = (stored > 0).then_some(stored);
        let saved_holds = state.holds.clone().unwrap_or_default();
        intake
            .restore(&tenant, state)
            .map_err(|error| Error::new(RESTORE_REFUSED, error.to_string()))?;
        Ok(Self {
            intake,
            cursor,
            store,
            tenant,
            hub_source,
            stored,
            saved_holds,
            dead: false,
            failpoints: None,
        })
    }

    /// The same intake with a crash simulator (tests only).
    #[must_use]
    pub fn with_failpoints(mut self, failpoints: Arc<dyn Failpoints>) -> Self {
        self.failpoints = Some(failpoints);
        self
    }

    /// The wrapped intake (counters, holds, repair list), for the host's health surface.
    pub fn intake(&self) -> &NodeIntake<A, P, C> {
        &self.intake
    }

    fn ready(&self) -> Result<()> {
        if self.dead {
            return Err(Error::new(
                NOT_RESTORED,
                "the intake stopped and was not restored again; nothing is acknowledged",
            ));
        }
        Ok(())
    }

    /// Stops the call at `point` when the simulator says so.
    fn crash(&mut self, point: Point) -> Result<()> {
        if self
            .failpoints
            .as_ref()
            .is_some_and(|failpoints| failpoints.crash_at(point))
        {
            self.dead = true;
            return Err(Error::apply_failed("the node stopped"));
        }
        Ok(())
    }

    fn receive_one(&mut self, served: Served, now_ms: u64, received: &mut Received) -> Result<()> {
        if served.tenant != self.tenant {
            // Another tenant's bundle: never applied here, and not settled.
            tracing::warn!(
                seq = served.stamp.seq,
                "a served bundle names another tenant"
            );
            received.quarantined += 1;
            return Ok(());
        }
        let seq = served.stamp.seq;
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
                    seq,
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
                    "the intake failed at serve sequence {seq}: {error}"
                )));
            }
        }
        Ok(())
    }

    /// Persists what a restart needs: the holds when they changed, then the repair items.
    fn persist(&mut self) -> Result<()> {
        if let Some(holds) = self.intake.holds(&self.tenant)
            && *holds != self.saved_holds
        {
            self.store.save_holds(&self.tenant, holds)?;
            self.saved_holds = holds.clone();
        }
        self.crash(Point::AfterHolds)?;
        let repair = self.intake.take_repair();
        if !repair.is_empty()
            && let Err(error) = self.store.push_repair(&repair)
        {
            // Nothing was stored: the items go back, so the next call tries again.
            self.intake.requeue_repair(repair);
            return Err(error);
        }
        self.crash(Point::AfterRepair)
    }
}

impl<A: BundleApply, P: ParkingStore, C: NodeCatalog, S: CursorStore, T: StateStore> Intake
    for DurableIntake<A, P, C, S, T>
{
    fn cursor(&mut self) -> Result<u64> {
        self.ready()?;
        Ok(self.stored)
    }

    fn receive(&mut self, items: Vec<Served>, quarantined: usize, now_ms: u64) -> Result<Received> {
        self.ready()?;
        let mut received = Received {
            quarantined,
            ..Received::default()
        };
        for (index, served) in items.into_iter().enumerate() {
            self.receive_one(served, now_ms, &mut received)?;
            self.crash(Point::AfterBundle(index))?;
        }
        self.intake.expire(now_ms);
        self.crash(Point::AfterReceive)?;
        self.persist()?;
        let prefix = self
            .intake
            .served_prefix(&self.tenant)
            .unwrap_or(0)
            .max(self.stored);
        if prefix > self.stored {
            self.cursor.store(prefix)?;
            self.stored = prefix;
        }
        received.prefix = prefix;
        Ok(received)
    }
}

// ---------------------------------------------------------------------------------------------
// What a restore reads that no store crate lists: the first position of every parked bundle.
// ---------------------------------------------------------------------------------------------

/// The statement that lists the parked bundles' first positions (the tenant predicate is in it,
/// as in every statement of this family).
pub const PARKED_FIRSTS: &str = "SELECT first_position FROM spot_sync_node_parked \
    WHERE tenant = $1 ORDER BY bundle_id";

/// Every statement this module runs, for the tenant-predicate scan.
pub const STATEMENTS: [&str; 1] = [PARKED_FIRSTS];

/// Deadline of the read.
pub const FIRSTS_DEADLINE: Duration = Duration::from_secs(30);

/// What went wrong inside the bridged read; mapped to a fixed message, never shown raw.
#[derive(Debug)]
enum FirstsFailure {
    Db,
    Decode,
}

impl From<tokio_postgres::Error> for FirstsFailure {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

/// The first position of every bundle parked for `tenant`, one entry per bundle (two bundles at
/// one position give two entries): what [`RestoreState::parked_first_positions`] needs.
///
/// The call is blocking and refuses to run inside an async runtime (`pg_bridge.wrong_thread`):
/// the intake factory runs on the worker's plain thread.
///
/// # Errors
///
/// `sync_node_host.store_failed` when the database cannot answer or a value is out of range;
/// a partial list is never returned.
pub fn parked_first_positions(bridge: &Bridge, tenant: &str) -> Result<Vec<u64>> {
    let tenant = tenant.to_owned();
    bridge
        .run(FIRSTS_DEADLINE, move |conn| {
            Box::pin(async move {
                let rows = conn.query(PARKED_FIRSTS, &[&tenant]).await?;
                rows.iter()
                    .map(|row| {
                        u64::try_from(row.get::<_, i64>(0)).map_err(|_| FirstsFailure::Decode)
                    })
                    .collect::<std::result::Result<Vec<u64>, FirstsFailure>>()
            })
        })
        .map_err(|error| match error.code() {
            Some(code) => Error::store_failed(format!("the database is not available ({code})")),
            None => Error::store_failed("the parked bundles could not be read"),
        })
}
