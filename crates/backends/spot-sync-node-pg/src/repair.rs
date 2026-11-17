//! [`PgRepairList`]: the repair list of the closure repair loop (WP-SYN-04.T2), kept across a
//! restart.
//!
//! The intake holds bundles that left parking without being applied (bound, expiry, oversize,
//! refusal) in memory and hands them out with `take_repair`. The host persists them here with
//! [`push`](PgRepairList::push) in the same step, so a restart does not forget a bundle the
//! prefix is held back for, and the repair loop reads [`list`](PgRepairList::list), asks the hub,
//! and [`remove`](PgRepairList::remove)s an item when its records are applied. Items count in the
//! recomputed applied prefix ([`crate::PgLedger::applied_prefix`]): they hold it back until
//! removed.

use std::time::Duration;

use spot_sync_closure::RecordKey;
use spot_sync_node::{RepairCause, RepairItem};

use crate::db::{Db, valid_tenant};
use crate::error::{OpError, PgNodeError, map, to_i64, to_u64};

/// Deadline of a repair-list call.
pub const REPAIR_DEADLINE: Duration = Duration::from_secs(15);

/// The most items one `list` call returns.
pub const MAX_LIST: usize = 1_000;

pub(crate) const PUSH: &str = "INSERT INTO spot_sync_node_repair (tenant, first_position, cause, missing_keys) \
    VALUES ($1, $2, $3, $4)";
pub(crate) const LIST: &str = "SELECT repair_id, first_position, cause, missing_keys \
    FROM spot_sync_node_repair WHERE tenant = $1 ORDER BY repair_id LIMIT $2";
pub(crate) const REMOVE: &str =
    "DELETE FROM spot_sync_node_repair WHERE tenant = $1 AND repair_id = $2";
pub(crate) const LEN: &str = "SELECT count(*) FROM spot_sync_node_repair WHERE tenant = $1";

fn cause_text(cause: RepairCause) -> &'static str {
    match cause {
        RepairCause::Bound => "bound",
        RepairCause::Expired => "expired",
        RepairCause::Oversize => "oversize",
        RepairCause::Refused => "refused",
    }
}

fn cause_of(text: &str) -> Result<RepairCause, OpError> {
    match text {
        "bound" => Ok(RepairCause::Bound),
        "expired" => Ok(RepairCause::Expired),
        "oversize" => Ok(RepairCause::Oversize),
        "refused" => Ok(RepairCause::Refused),
        _ => Err(OpError::Decode),
    }
}

/// An item with the id the list gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRepair {
    /// The id to pass to [`PgRepairList::remove`].
    pub id: i64,
    /// The item.
    pub item: RepairItem,
}

/// The repair list.
#[derive(Clone, Debug)]
pub struct PgRepairList {
    db: Db,
}

impl PgRepairList {
    /// A repair list on `db`.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Stores the items in one transaction, in order.
    ///
    /// # Errors
    /// `sync_node_pg.state_invalid` for an invalid tenant or a value past the column; the store's
    /// failure (nothing is stored then).
    pub fn push(&self, items: &[RepairItem]) -> Result<(), PgNodeError> {
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            if !valid_tenant(&item.tenant) {
                return Err(PgNodeError::StateInvalid);
            }
            let first = to_i64(item.first_position).map_err(|_| PgNodeError::StateInvalid)?;
            let missing =
                serde_json::to_string(&item.missing).map_err(|_| PgNodeError::StateInvalid)?;
            rows.push((item.tenant.clone(), first, cause_text(item.cause), missing));
        }
        self.db
            .run(REPAIR_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    for (tenant, first, cause, missing) in &rows {
                        tx.execute(PUSH, &[tenant, first, cause, missing]).await?;
                    }
                    tx.commit().await?;
                    Ok(())
                })
            })
            .map_err(map)
    }

    /// The oldest items of `tenant`, at most `limit` (and at most [`MAX_LIST`]).
    ///
    /// # Errors
    /// The store's failure; `sync_node_pg.state_invalid` for a damaged row.
    pub fn list(&self, tenant: &str, limit: usize) -> Result<Vec<StoredRepair>, PgNodeError> {
        let limit = i64::try_from(limit.min(MAX_LIST)).unwrap_or(1_000);
        let tenant = tenant.to_owned();
        self.db
            .run(REPAIR_DEADLINE, move |conn| {
                Box::pin(async move {
                    let mut out = Vec::new();
                    for row in conn.query(LIST, &[&tenant, &limit]).await? {
                        let missing: Vec<RecordKey> =
                            serde_json::from_str(&row.get::<_, String>(3))
                                .map_err(|_| OpError::Decode)?;
                        out.push(StoredRepair {
                            id: row.get(0),
                            item: RepairItem {
                                tenant: tenant.clone(),
                                first_position: to_u64(row.get::<_, i64>(1))?,
                                missing,
                                cause: cause_of(&row.get::<_, String>(2))?,
                            },
                        });
                    }
                    Ok(out)
                })
            })
            .map_err(map)
    }

    /// Removes one item of `tenant`; whether it was there.
    ///
    /// # Errors
    /// The store's failure.
    pub fn remove(&self, tenant: &str, id: i64) -> Result<bool, PgNodeError> {
        let tenant = tenant.to_owned();
        self.db
            .run(REPAIR_DEADLINE, move |conn| {
                Box::pin(async move { Ok(conn.execute(REMOVE, &[&tenant, &id]).await? > 0) })
            })
            .map_err(map)
    }

    /// Items waiting for `tenant`.
    ///
    /// # Errors
    /// The store's failure.
    pub fn len(&self, tenant: &str) -> Result<u64, PgNodeError> {
        let tenant = tenant.to_owned();
        self.db
            .run(REPAIR_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(LEN, &[&tenant]).await?;
                    to_u64(row.get::<_, i64>(0))
                })
            })
            .map_err(map)
    }

    /// Whether nothing waits for `tenant`.
    ///
    /// # Errors
    /// The store's failure.
    pub fn is_empty(&self, tenant: &str) -> Result<bool, PgNodeError> {
        Ok(self.len(tenant)? == 0)
    }
}
