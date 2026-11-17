//! [`PgParkingStore`]: `ParkingStore` over `spot_sync_node_parked`.
//!
//! One queue per tenant, never mixed: every statement filters on the tenant. Bundles come back in
//! **arrival order**, which is the insertion order of the table (`bundle_id`), not the intake's
//! `seq`: the intake numbers arrivals from zero in every process, so after a restart its numbers
//! repeat, while the insertion order never does.
//!
//! The count and byte bounds are the intake's: it asks [`len`](ParkingStore::len) and
//! [`bytes`](ParkingStore::bytes) before it parks and pushes the oldest bundle out with
//! [`take_oldest`](ParkingStore::take_oldest) onto the repair list (counted, never silently
//! dropped); [`crate::PgRepairList`] keeps that list across a restart.
//!
//! Age is the **database's** clock: the intake's `now_ms` is a monotonic count that restarts with
//! the process, so a bundle parked before a restart could never look old by it. `expire` therefore
//! ignores `now_ms` and removes bundles parked more than `max_age_ms` ago by `parked_at`.
//!
//! A failed call cannot return an error through the trait: it is recorded for `take_failure` and
//! answers in the safe direction (a bundle that was not parked is redelivered by the hub, a
//! bundle that was not released stays parked).

use std::collections::BTreeSet;
use std::time::Duration;

use spot_sync_closure::{Bundle, RecordKey};
use spot_sync_node::{ParkedBundle, ParkingStore, ServeStamp};
use tokio_postgres::Row;

use crate::db::{Db, valid_tenant};
use crate::error::{FailureLog, OpError, PgNodeError, map, to_i64, to_u64};

/// Deadline of a parking call.
pub const PARKING_DEADLINE: Duration = Duration::from_secs(30);

pub(crate) const PARK: &str = "INSERT INTO spot_sync_node_parked \
    (tenant, arrived_seq, bytes, first_position, serve_seq, serve_after, parked_at_ms, bundle, missing_keys) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";
pub(crate) const MISSING_LIST: &str = "SELECT bundle_id, missing_keys FROM spot_sync_node_parked \
    WHERE tenant = $1 ORDER BY bundle_id FOR UPDATE";
pub(crate) const SET_MISSING: &str = "UPDATE spot_sync_node_parked SET missing_keys = $3 \
    WHERE tenant = $1 AND bundle_id = $2";
pub(crate) const REMOVE_ONE: &str = "DELETE FROM spot_sync_node_parked \
    WHERE tenant = $1 AND bundle_id = $2 RETURNING arrived_seq, bytes, first_position, serve_seq, \
    serve_after, parked_at_ms, bundle, missing_keys";
pub(crate) const TAKE_OLDEST: &str = "DELETE FROM spot_sync_node_parked WHERE tenant = $1 AND bundle_id = (\
    SELECT bundle_id FROM spot_sync_node_parked WHERE tenant = $1 \
    ORDER BY bundle_id LIMIT 1 FOR UPDATE SKIP LOCKED) \
    RETURNING arrived_seq, bytes, first_position, serve_seq, serve_after, parked_at_ms, bundle, missing_keys";
pub(crate) const SIZE: &str = "SELECT count(*), COALESCE(sum(bytes), 0)::bigint \
    FROM spot_sync_node_parked WHERE tenant = $1";
pub(crate) const EXPIRE: &str = "DELETE FROM spot_sync_node_parked WHERE tenant = $1 \
    AND parked_at < now() - ($2::bigint * interval '1 millisecond') \
    RETURNING arrived_seq, bytes, first_position, serve_seq, serve_after, parked_at_ms, bundle, missing_keys";
/// The tenants that have parked bundles: the one statement without a tenant predicate, because it
/// finds the tenants `expire` then visits one by one.
pub(crate) const TENANTS: &str = "SELECT DISTINCT tenant FROM spot_sync_node_parked";

/// The parking store. One per node; the tenant is an argument of every call.
#[derive(Clone, Debug)]
pub struct PgParkingStore {
    db: Db,
    failures: FailureLog,
}

impl PgParkingStore {
    /// A parking store on `db`.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            db,
            failures: FailureLog::new(),
        }
    }

    /// The last failure of a trait method since the previous call.
    #[must_use]
    pub fn take_failure(&self) -> Option<PgNodeError> {
        self.failures.take()
    }

    fn note<T>(&self, result: Result<T, spot_pg_bridge::BridgeError<OpError>>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.failures.record(map(error));
                None
            }
        }
    }
}

/// A parked bundle as a row of `COLUMNS`.
fn decode(tenant: &str, row: &Row) -> Result<ParkedBundle, OpError> {
    let serve = match (row.get::<_, Option<i64>>(3), row.get::<_, Option<i64>>(4)) {
        (Some(seq), Some(after)) => Some(ServeStamp {
            seq: to_u64(seq)?,
            after: to_u64(after)?,
        }),
        _ => None,
    };
    let bundle: Bundle =
        serde_json::from_str(&row.get::<_, String>(6)).map_err(|_| OpError::Decode)?;
    let missing: Vec<RecordKey> =
        serde_json::from_str(&row.get::<_, String>(7)).map_err(|_| OpError::Decode)?;
    Ok(ParkedBundle {
        seq: to_u64(row.get::<_, i64>(0))?,
        tenant: tenant.to_owned(),
        bundle,
        missing: missing.into_iter().collect(),
        parked_at_ms: to_u64(row.get::<_, i64>(5))?,
        first_position: to_u64(row.get::<_, i64>(2))?,
        bytes: usize::try_from(row.get::<_, i64>(1)).map_err(|_| OpError::Decode)?,
        serve,
    })
}

impl ParkingStore for PgParkingStore {
    fn park(&mut self, parked: ParkedBundle) {
        if !valid_tenant(&parked.tenant) {
            self.failures.record(PgNodeError::UnsupportedContext {
                reason: "the parked bundle names an invalid tenant",
            });
            return;
        }
        let row = (|| -> Result<_, OpError> {
            let missing: Vec<&RecordKey> = parked.missing.iter().collect();
            Ok((
                to_i64(parked.seq)?,
                i64::try_from(parked.bytes).map_err(|_| OpError::Decode)?,
                to_i64(parked.first_position)?,
                parked.serve.map(|s| to_i64(s.seq)).transpose()?,
                parked.serve.map(|s| to_i64(s.after)).transpose()?,
                to_i64(parked.parked_at_ms)?,
                serde_json::to_string(&parked.bundle).map_err(|_| OpError::Decode)?,
                serde_json::to_string(&missing).map_err(|_| OpError::Decode)?,
            ))
        })();
        let Ok((seq, bytes, first, serve_seq, serve_after, at, bundle, missing)) = row else {
            self.failures.record(PgNodeError::StateInvalid);
            return;
        };
        let tenant = parked.tenant;
        let result = self.db.run(PARKING_DEADLINE, move |conn| {
            Box::pin(async move {
                conn.execute(
                    PARK,
                    &[
                        &tenant,
                        &seq,
                        &bytes,
                        &first,
                        &serve_seq,
                        &serve_after,
                        &at,
                        &bundle,
                        &missing,
                    ],
                )
                .await?;
                Ok(())
            })
        });
        self.note(result);
    }

    fn ready(&mut self, tenant: &str, applied_keys: &[RecordKey]) -> Vec<ParkedBundle> {
        if !valid_tenant(tenant) || applied_keys.is_empty() {
            return Vec::new();
        }
        let tenant = tenant.to_owned();
        let applied: BTreeSet<RecordKey> = applied_keys.iter().cloned().collect();
        let result = self.db.run(PARKING_DEADLINE, move |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                let rows = tx.query(MISSING_LIST, &[&tenant]).await?;
                let mut ready = Vec::new();
                for row in rows {
                    let id: i64 = row.get(0);
                    let missing: Vec<RecordKey> = serde_json::from_str(&row.get::<_, String>(1))
                        .map_err(|_| OpError::Decode)?;
                    let before = missing.len();
                    let left: Vec<&RecordKey> =
                        missing.iter().filter(|k| !applied.contains(*k)).collect();
                    if left.is_empty() {
                        let gone = tx.query_one(REMOVE_ONE, &[&tenant, &id]).await?;
                        ready.push(decode(&tenant, &gone)?);
                    } else if left.len() != before {
                        let text = serde_json::to_string(&left).map_err(|_| OpError::Decode)?;
                        tx.execute(SET_MISSING, &[&tenant, &id, &text]).await?;
                    }
                }
                tx.commit().await?;
                Ok(ready)
            })
        });
        self.note(result).unwrap_or_default()
    }

    fn take_oldest(&mut self, tenant: &str) -> Option<ParkedBundle> {
        if !valid_tenant(tenant) {
            return None;
        }
        let tenant = tenant.to_owned();
        let result = self.db.run(PARKING_DEADLINE, move |conn| {
            Box::pin(async move {
                let row = conn.query_opt(TAKE_OLDEST, &[&tenant]).await?;
                row.map(|r| decode(&tenant, &r)).transpose()
            })
        });
        self.note(result).flatten()
    }

    fn len(&self, tenant: &str) -> usize {
        self.size(tenant).0
    }

    fn bytes(&self, tenant: &str) -> usize {
        self.size(tenant).1
    }

    fn expire(&mut self, _now_ms: u64, max_age_ms: u64) -> Vec<ParkedBundle> {
        let Ok(max_age) = i64::try_from(max_age_ms) else {
            return Vec::new();
        };
        let result = self.db.run(PARKING_DEADLINE, move |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                let tenants: Vec<String> = tx
                    .query(TENANTS, &[])
                    .await?
                    .iter()
                    .map(|r| r.get::<_, String>(0))
                    .collect();
                let mut expired = Vec::new();
                for tenant in tenants {
                    for row in tx.query(EXPIRE, &[&tenant, &max_age]).await? {
                        expired.push(decode(&tenant, &row)?);
                    }
                }
                tx.commit().await?;
                Ok(expired)
            })
        });
        let mut expired: Vec<ParkedBundle> = self.note(result).unwrap_or_default();
        expired.sort_by_key(|p| p.seq);
        expired
    }
}

impl PgParkingStore {
    /// `(bundles, image bytes)` parked for the tenant; `(0, 0)` on a failure (recorded).
    fn size(&self, tenant: &str) -> (usize, usize) {
        if !valid_tenant(tenant) {
            return (0, 0);
        }
        let tenant = tenant.to_owned();
        let result = self.db.run(PARKING_DEADLINE, move |conn| {
            Box::pin(async move {
                let row = conn.query_one(SIZE, &[&tenant]).await?;
                let count = usize::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
                let bytes = usize::try_from(row.get::<_, i64>(1)).map_err(|_| OpError::Decode)?;
                Ok((count, bytes))
            })
        });
        self.note(result).unwrap_or((0, 0))
    }
}
