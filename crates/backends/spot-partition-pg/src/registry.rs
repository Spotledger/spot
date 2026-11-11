//! [`PgPartitionRegistry`]: the effective-dated, audited ownership assignments in PostgreSQL.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_partition::{
    Assignment, AssignmentRequest, PartitionChange, PartitionError, PartitionMap,
    PartitionRegistry, ScopeKey,
};
use spot_pg_bridge::Bridge;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::codec::{decode_instant, decode_version, encode_instant};
use crate::error::{OpError, PgPartitionError, map};
use crate::{check_context, prepare_all};

/// Deadline of an assignment: one lock, three statements, one commit.
pub const ASSIGN_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the map snapshot read.
pub const MAP_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the history read, which returns every change.
pub const HISTORY_DEADLINE: Duration = Duration::from_secs(30);

/// The partition registry over PostgreSQL.
///
/// See the crate documentation for where it may be used. Two registries (two processes, two
/// hubs) over the same database never both own a scope key in overlapping intervals: `assign`
/// serialises on a per-scope-key advisory lock and checks the overlap in the same transaction as
/// the insert.
pub struct PgPartitionRegistry {
    bridge: Arc<Bridge>,
    read_failed: Mutex<Option<PartitionError>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The text the per-scope advisory lock is keyed by. A scope key part holds no control
/// character, so the unit separator makes the pair unambiguous.
fn lock_key(scope: &ScopeKey) -> String {
    format!("{}\u{1f}{}", scope.key_type(), scope.value())
}

fn decode_scope(key_type: &str, value: &str) -> Result<ScopeKey, OpError> {
    ScopeKey::new(key_type, value).map_err(|_| OpError::Decode)
}

fn decode_node(text: &str) -> Result<NodeId, OpError> {
    NodeId::new(text).map_err(|_| OpError::Decode)
}

impl PgPartitionRegistry {
    /// Builds the registry over `bridge`. `pool_size` is the size of the pool the bridge was
    /// built on; no I/O happens here (call [`PgPartitionRegistry::prepare`] once at start-up).
    ///
    /// # Errors
    /// `partition_pg.unsupported_context` when the calling thread is inside a tokio runtime (the
    /// bridge would refuse every call), or when the bridge allows more calls in flight than the
    /// pool can serve.
    pub fn new(bridge: Arc<Bridge>, pool_size: usize) -> Result<Self, PgPartitionError> {
        check_context(&bridge, pool_size)?;
        Ok(Self {
            bridge,
            read_failed: Mutex::new(None),
        })
    }

    /// Creates the tables if they are missing. Idempotent; shared with the allocator.
    ///
    /// # Errors
    /// `partition_pg.prepare_failed` when the DDL cannot be applied or the bridge refuses.
    pub fn prepare(&self) -> Result<(), PgPartitionError> {
        prepare_all(&self.bridge)
    }

    /// The ownership map effective at `at`, or the failure.
    ///
    /// One statement, so one consistent snapshot.
    ///
    /// # Errors
    /// [`PartitionError::Store`] when the database or the bridge fails.
    pub fn try_map_at(&self, at: OffsetDateTime) -> Result<PartitionMap, PartitionError> {
        let at = encode_instant(at);
        self.bridge
            .run(MAP_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn
                        .query(
                            "SELECT scope_type, scope_value, node, version, \
                                    effective_from::text, effective_to::text \
                             FROM spot_partition_assignment \
                             WHERE effective_from <= $1::text::numeric \
                               AND (effective_to IS NULL OR $1::text::numeric < effective_to) \
                             ORDER BY id",
                            &[&at],
                        )
                        .await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        out.push(Assignment {
                            scope: decode_scope(row.get(0), row.get(1))?,
                            node: decode_node(row.get(2))?,
                            version: decode_version(row.get(3))?,
                            effective_from: decode_instant(row.get(4))?,
                            effective_to: row
                                .get::<_, Option<&str>>(5)
                                .map(decode_instant)
                                .transpose()?,
                        });
                    }
                    Ok::<_, OpError>(PartitionMap::new(out))
                })
            })
            .map_err(map)
    }

    /// The audit trail, oldest first, or the failure.
    ///
    /// # Errors
    /// [`PartitionError::Store`] when the database or the bridge fails.
    pub fn try_history(&self) -> Result<Vec<PartitionChange>, PartitionError> {
        self.bridge
            .run(HISTORY_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn
                        .query(
                            "SELECT scope_type, scope_value, node, version, \
                                    effective_from::text, effective_to::text, \
                                    actor, reason, recorded_at::text \
                             FROM spot_partition_history ORDER BY seq",
                            &[],
                        )
                        .await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        out.push(PartitionChange {
                            scope: decode_scope(row.get(0), row.get(1))?,
                            node: decode_node(row.get(2))?,
                            version: decode_version(row.get(3))?,
                            effective_from: decode_instant(row.get(4))?,
                            effective_to: row
                                .get::<_, Option<&str>>(5)
                                .map(decode_instant)
                                .transpose()?,
                            actor: row.get::<_, String>(6),
                            reason: row.get::<_, String>(7),
                            at: decode_instant(row.get(8))?,
                        });
                    }
                    Ok::<_, OpError>(out)
                })
            })
            .map_err(map)
    }

    /// The first failure of a trait read ([`PartitionRegistry::map_at`] or
    /// [`PartitionRegistry::history`]) since the last call, clearing it.
    ///
    /// The trait's reads cannot report a failure, so a failed read returns an **empty** answer
    /// (see the `PartitionRegistry` impl) and is recorded here. A caller that must tell "no
    /// owner" from "database down" uses [`try_map_at`](Self::try_map_at) and
    /// [`try_history`](Self::try_history), or checks this after a read.
    pub fn take_read_failure(&self) -> Option<PartitionError> {
        lock(&self.read_failed).take()
    }

    fn latch<T: Default>(&self, result: Result<T, PartitionError>) -> T {
        result.unwrap_or_else(|error| {
            let mut slot = lock(&self.read_failed);
            if slot.is_none() {
                *slot = Some(error);
            }
            T::default()
        })
    }
}

impl PartitionRegistry for PgPartitionRegistry {
    fn assign(&self, request: AssignmentRequest) -> Result<u64, PartitionError> {
        if let Some(to) = request.effective_to
            && to <= request.effective_from
        {
            return Err(PartitionError::InvalidArgument {
                reason: "effective_to is not after effective_from".to_owned(),
            });
        }
        let key = lock_key(&request.scope);
        let from = encode_instant(request.effective_from);
        let to = request.effective_to.map(encode_instant);
        let at = encode_instant(request.at);
        self.bridge
            .run(ASSIGN_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let scope = &request.scope;
                    let (scope_type, scope_value) = (scope.key_type(), scope.value());
                    let node = request.node.as_str();
                    let tx = conn.transaction().await?;
                    // One assignment at a time per scope key, across processes. Held to the end
                    // of the transaction, so the overlap check and the insert are one step.
                    tx.execute(
                        "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
                        &[&key],
                    )
                    .await?;
                    let conflicting = tx
                        .query_opt(
                            "SELECT node FROM spot_partition_assignment \
                             WHERE scope_type = $1 AND scope_value = $2 AND node <> $3 \
                               AND (effective_to IS NULL \
                                    OR $4::text::numeric < effective_to) \
                               AND ($5::text IS NULL \
                                    OR effective_from < $5::text::numeric) \
                             ORDER BY version LIMIT 1",
                            &[&scope_type, &scope_value, &node, &from, &to],
                        )
                        .await?;
                    if let Some(row) = conflicting {
                        return Err(OpError::Refused(PartitionError::OwnershipConflict {
                            scope: scope.to_string(),
                            owner: row.get::<_, String>(0),
                            challenger: node.to_owned(),
                        }));
                    }
                    let version: i64 = tx
                        .query_one(
                            "SELECT COALESCE(MAX(version), 0) + 1 FROM spot_partition_assignment \
                             WHERE scope_type = $1 AND scope_value = $2",
                            &[&scope_type, &scope_value],
                        )
                        .await?
                        .get(0);
                    tx.execute(
                        "INSERT INTO spot_partition_assignment \
                         (scope_type, scope_value, node, version, effective_from, effective_to) \
                         VALUES ($1, $2, $3, $4, $5::text::numeric, $6::text::numeric)",
                        &[&scope_type, &scope_value, &node, &version, &from, &to],
                    )
                    .await?;
                    tx.execute(
                        "INSERT INTO spot_partition_history \
                         (scope_type, scope_value, node, version, effective_from, effective_to, \
                          actor, reason, recorded_at) \
                         VALUES ($1, $2, $3, $4, $5::text::numeric, $6::text::numeric, \
                                 $7, $8, $9::text::numeric)",
                        &[
                            &scope_type,
                            &scope_value,
                            &node,
                            &version,
                            &from,
                            &to,
                            &request.actor,
                            &request.reason,
                            &at,
                        ],
                    )
                    .await?;
                    tx.commit().await?;
                    decode_version(version)
                })
            })
            .map_err(map)
    }

    /// The ownership map effective at `at`.
    ///
    /// The trait cannot report a failure. When the read fails this returns an **empty map**,
    /// which fails closed (`OwnerCheck` refuses every write and replica when no node owns a
    /// key), and records the failure for [`PgPartitionRegistry::take_read_failure`]. It never
    /// presents a database that is down as a map with owners.
    fn map_at(&self, at: OffsetDateTime) -> PartitionMap {
        self.latch(self.try_map_at(at))
    }

    /// The audit trail, oldest first. On a failed read this returns an empty trail and records
    /// the failure for [`PgPartitionRegistry::take_read_failure`]; use
    /// [`PgPartitionRegistry::try_history`] to get the error.
    fn history(&self) -> Vec<PartitionChange> {
        self.latch(self.try_history())
    }
}
