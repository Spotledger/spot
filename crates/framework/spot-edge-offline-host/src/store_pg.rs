//! The durable leased ranges: `spot_leased_range` on the shared bridge, as the library's
//! [`HighWaterStore`].
//!
//! # The table
//!
//! `spot_leased_range(tenant, series, node, lease_id, first_number, last_number, next_number)`,
//! one row per tenant, series and node. The tenant predicate is in every statement
//! ([`STATEMENTS`]); the host creates nothing at start-up (the table comes from `spot migrate`
//! through the extra-table hook, which reads [`DDL`]): [`PgRangeStore::require_table`] says to
//! run it when the table is missing.
//!
//! # A number is never reused: the reserved high-water mark
//!
//! [`BranchSeries::allocate`] saves the high-water mark **before** it returns a number, and the
//! naming path calls it inside a request. A database round trip per document would be a cost on
//! every write, so the stored `next_number` is a **reservation**: it is always at or above the
//! next number to hand out, by at most [`STRIDE`]. A save below the reservation touches nothing;
//! a save that crosses it writes a new reservation `STRIDE` numbers ahead (capped at the end of
//! the lease). After a crash the series resumes at the reservation, so the numbers between the
//! last one handed out and the reservation are **skipped (a gap), never reissued**. A gap in a
//! leased series is the documented rollback rule of the library; a duplicate is the failure.
//!
//! The write is guarded in SQL: a reservation only ever rises within one lease, so a racing
//! writer cannot lower it. A new lease replaces a row only through [`save_if_above`], which reads
//! the row `FOR UPDATE`, decides with the library's own rule and writes in the same transaction.
//!
//! [`save_if_above`]: HighWaterStore::save_if_above

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_partition::{
    HighWaterStore, InstallDecision, LeaseId, PartitionError, RangeLease, StoredSeries,
};
use spot_pg_bridge::Bridge;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

use crate::blocking::{Op, failure_text, off_runtime};

/// The table's idempotent DDL, one statement per object, for the migrator's extra-table hook.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_leased_range (\
    tenant text NOT NULL, \
    series text NOT NULL, \
    node text NOT NULL, \
    lease_id bigint NOT NULL CHECK (lease_id >= 0), \
    first_number numeric(39,0) NOT NULL CHECK (first_number >= 1), \
    last_number numeric(39,0) NOT NULL, \
    next_number numeric(39,0) NOT NULL, \
    updated_at timestamptz NOT NULL DEFAULT now(), \
    PRIMARY KEY (tenant, series, node), \
    CHECK (last_number >= first_number AND next_number >= first_number \
        AND next_number <= last_number + 1))";

/// The tables this module owns.
pub const TABLES: [&str; 1] = ["spot_leased_range"];

/// Every statement the module runs, for the tenant-predicate scan.
pub const STATEMENTS: [&str; 5] = [LOAD, LOCK, UPSERT, RAISE, MISSING];

const LOAD: &str = "SELECT lease_id, first_number::text, last_number::text, next_number::text \
    FROM spot_leased_range WHERE tenant = $1 AND series = $2 AND node = $3";
const LOCK: &str = "SELECT lease_id, first_number::text, last_number::text, next_number::text \
    FROM spot_leased_range WHERE tenant = $1 AND series = $2 AND node = $3 FOR UPDATE";
const UPSERT: &str = "INSERT INTO spot_leased_range \
    (tenant, series, node, lease_id, first_number, last_number, next_number) \
    VALUES ($1, $2, $3, $4, $5::text::numeric, $6::text::numeric, $7::text::numeric) \
    ON CONFLICT (tenant, series, node) DO UPDATE SET lease_id = EXCLUDED.lease_id, \
    first_number = EXCLUDED.first_number, last_number = EXCLUDED.last_number, \
    next_number = EXCLUDED.next_number, updated_at = now()";
const RAISE: &str = "UPDATE spot_leased_range SET next_number = $5::text::numeric, \
    updated_at = now() WHERE tenant = $1 AND series = $2 AND node = $3 AND lease_id = $4 \
    AND next_number < $5::text::numeric";
const MISSING: &str = "SELECT to_regclass('public.spot_leased_range') IS NOT NULL";

/// Numbers reserved ahead of the one being handed out.
pub const STRIDE: u64 = 100;
/// Deadline of one bridged call.
pub const DEADLINE: Duration = Duration::from_secs(10);

type Row = (i64, String, String, String);

/// What the store knows is durable for one series and node.
#[derive(Clone, Copy, Debug)]
struct Durable {
    lease_id: u64,
    reserved: u64,
}

/// [`HighWaterStore`] over `spot_leased_range`, for one tenant.
pub struct PgRangeStore {
    bridge: Arc<Bridge>,
    tenant: String,
    stride: u64,
    durable: Mutex<BTreeMap<(String, String), Durable>>,
    read_failure: Mutex<Option<String>>,
}

impl std::fmt::Debug for PgRangeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgRangeStore")
            .field("tenant", &self.tenant)
            .field("stride", &self.stride)
            .finish_non_exhaustive()
    }
}

impl PgRangeStore {
    /// The store of `tenant`. No I/O happens here. `stride` is clamped to at least 1 (1 means a
    /// write per number).
    pub fn new(bridge: Arc<Bridge>, tenant: impl Into<String>, stride: u64) -> Self {
        Self {
            bridge,
            tenant: tenant.into(),
            stride: stride.max(1),
            durable: Mutex::new(BTreeMap::new()),
            read_failure: Mutex::new(None),
        }
    }

    /// Fails when the table does not exist: the host refuses to run the module and says to run
    /// `spot migrate`; it never creates the table itself.
    ///
    /// # Errors
    ///
    /// A message for the operator.
    pub fn require_table(&self) -> Result<(), String> {
        let bridge = Arc::clone(&self.bridge);
        let present = off_runtime(move || {
            bridge.run(DEADLINE, |conn| {
                Box::pin(async move {
                    let row = conn.query_one(MISSING, &[]).await?;
                    Ok::<bool, Op>(row.get::<_, bool>(0))
                })
            })
        })
        .ok_or_else(|| "the table check did not finish".to_owned())?
        .map_err(|error| failure_text(&error))?;
        if present {
            Ok(())
        } else {
            Err("spot_leased_range is missing: run `spot migrate`".to_owned())
        }
    }

    /// The failure of the last read that could not be answered (a read cannot fail in the trait),
    /// taken once.
    pub fn take_read_failure(&self) -> Option<String> {
        self.read_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    fn note(&self, durable: Durable, series: &str, node: &str) {
        self.durable
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((series.to_owned(), node.to_owned()), durable);
    }

    fn cached(&self, series: &str, node: &str) -> Option<Durable> {
        self.durable
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(series.to_owned(), node.to_owned()))
            .copied()
    }

    fn store_error(why: impl Into<String>) -> PartitionError {
        PartitionError::Store { reason: why.into() }
    }
}

fn parse_row(series: &SeriesKey, node: &NodeId, row: &Row) -> Result<StoredSeries, Op> {
    let lease_id = u64::try_from(row.0).map_err(|_| Op::Decode)?;
    let from: u64 = row.1.parse().map_err(|_| Op::Decode)?;
    let to: u64 = row.2.parse().map_err(|_| Op::Decode)?;
    let next: u64 = row.3.parse().map_err(|_| Op::Decode)?;
    let lease = RangeLease::new(
        series.clone(),
        from,
        to,
        node.clone(),
        LeaseId::new(lease_id),
    )
    .map_err(|_| Op::Decode)?;
    Ok(StoredSeries { lease, next })
}

impl HighWaterStore for PgRangeStore {
    fn load(&self, series: &SeriesKey, node: &NodeId) -> Option<StoredSeries> {
        let (bridge, tenant) = (Arc::clone(&self.bridge), self.tenant.clone());
        let (key, node_text) = (series.as_str().to_owned(), node.as_str().to_owned());
        let outcome = off_runtime(move || {
            bridge.run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(LOAD, &[&tenant, &key, &node_text]).await?;
                    Ok::<Option<Row>, Op>(
                        rows.first()
                            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))),
                    )
                })
            })
        });
        let row = match outcome {
            Some(Ok(row)) => row,
            Some(Err(error)) => {
                *self.read_failure.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(failure_text(&error));
                return None;
            }
            None => return None,
        };
        let stored = parse_row(series, node, &row?).ok()?;
        self.note(
            Durable {
                lease_id: stored.lease.lease_id.get(),
                reserved: stored.next,
            },
            series.as_str(),
            node.as_str(),
        );
        Some(stored)
    }

    fn save(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<(), PartitionError> {
        let lease_id = state.lease.lease_id.get();
        let known = self.cached(series.as_str(), node.as_str());
        if known.is_some_and(|k| k.lease_id == lease_id && state.next <= k.reserved) {
            // Inside the reservation: the stored mark is already at or above this number.
            return Ok(());
        }
        let wanted = state
            .next
            .saturating_add(self.stride - 1)
            .min(state.lease.to.saturating_add(1))
            .max(state.next);
        let signed = i64::try_from(lease_id)
            .map_err(|_| Self::store_error("the lease id does not fit the table"))?;
        let (bridge, tenant) = (Arc::clone(&self.bridge), self.tenant.clone());
        let (key, node_text) = (series.as_str().to_owned(), node.as_str().to_owned());
        let wanted_text = wanted.to_string();
        let raised = off_runtime(move || {
            bridge.run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let n = conn
                        .execute(RAISE, &[&tenant, &key, &node_text, &signed, &wanted_text])
                        .await?;
                    Ok::<u64, Op>(n)
                })
            })
        })
        .ok_or_else(|| Self::store_error("the reservation write did not finish"))?
        .map_err(|error| Self::store_error(failure_text(&error)))?;
        if raised == 0 {
            // The row is for another lease, missing, or already at or above the wanted mark:
            // none of these may be papered over. Fail closed: no number is handed out.
            return Err(Self::store_error(
                "the stored reservation could not be raised (another lease, or a lower mark)",
            ));
        }
        self.note(
            Durable {
                lease_id,
                reserved: wanted,
            },
            series.as_str(),
            node.as_str(),
        );
        Ok(())
    }

    fn save_if_above(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<u64, PartitionError> {
        let lease_id = i64::try_from(state.lease.lease_id.get())
            .map_err(|_| Self::store_error("the lease id does not fit the table"))?;
        let (bridge, tenant) = (Arc::clone(&self.bridge), self.tenant.clone());
        let (key, node_text) = (series.as_str().to_owned(), node.as_str().to_owned());
        let (series_for_row, node_for_row) = (series.clone(), node.clone());
        let wanted = state.next.min(state.lease.to.saturating_add(1));
        let lease = state.lease.clone();
        let decided = off_runtime(move || {
            bridge.run(DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let rows = tx.query(LOCK, &[&tenant, &key, &node_text]).await?;
                    let stored = match rows.first() {
                        Some(r) => Some(parse_row(
                            &series_for_row,
                            &node_for_row,
                            &(r.get(0), r.get(1), r.get(2), r.get(3)),
                        )?),
                        None => None,
                    };
                    let decision = match &stored {
                        Some(stored) => stored.install_decision(&lease),
                        None => Some(InstallDecision::Advance(wanted)),
                    };
                    let outcome = match decision {
                        Some(InstallDecision::Resume(next)) => Ok((next, lease.lease_id.get())),
                        Some(InstallDecision::Advance(next)) => {
                            tx.execute(
                                UPSERT,
                                &[
                                    &tenant,
                                    &key,
                                    &node_text,
                                    &lease_id,
                                    &lease.from.to_string(),
                                    &lease.to.to_string(),
                                    &next.to_string(),
                                ],
                            )
                            .await?;
                            Ok((next, lease.lease_id.get()))
                        }
                        None => Err(Op::Refused),
                    };
                    tx.commit().await?;
                    outcome
                })
            })
        })
        .ok_or_else(|| Self::store_error("the install did not finish"))?;
        match decided {
            Ok((next, id)) => {
                self.note(
                    Durable {
                        lease_id: id,
                        reserved: next,
                    },
                    series.as_str(),
                    node.as_str(),
                );
                Ok(next)
            }
            Err(error) => match error {
                spot_pg_bridge::BridgeError::Store(Op::Refused) => {
                    Err(PartitionError::LeaseNotAboveStored {
                        series: series.as_str().to_owned(),
                        node: node.to_string(),
                    })
                }
                other => Err(Self::store_error(failure_text(&other))),
            },
        }
    }
}
