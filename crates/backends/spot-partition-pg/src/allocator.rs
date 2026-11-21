//! [`PgRangeAllocator`]: naming ranges granted from a cursor per series, in PostgreSQL.
//!
//! A reissued range duplicates document numbers, so the grant is one transaction: lock the
//! series row, read the cursor, advance it, insert the grant row, commit. Two allocators (two
//! hubs, two threads) therefore never grant overlapping numbers, and a transaction that rolls
//! back leaves the cursor where it was: a number is never skipped or reissued without a
//! committed grant row recording it.

use std::sync::Arc;
use std::time::Duration;

use spot_partition::{LeaseId, PartitionError, RangeAllocator, RangeGrant, RangeLease};
use spot_pg_bridge::Bridge;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::codec::{decode_instant, decode_u64, decode_u128, encode_instant};
use crate::error::{OpError, PgPartitionError, map};
use crate::{check_context, prepare_all};

/// Deadline of one grant: one row lock, one update, one insert, one commit.
pub const LEASE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the grants read, which returns every grant.
pub const GRANTS_DEADLINE: Duration = Duration::from_secs(30);

/// The range allocator over PostgreSQL.
pub struct PgRangeAllocator {
    bridge: Arc<Bridge>,
}

impl PgRangeAllocator {
    /// Builds the allocator over `bridge`. `pool_size` is the size of the pool the bridge was
    /// built on; no I/O happens here (call [`PgRangeAllocator::prepare`] once at start-up).
    ///
    /// # Errors
    /// `partition_pg.unsupported_context` when the calling thread is inside a tokio runtime, or
    /// when the bridge allows more calls in flight than the pool can serve.
    pub fn new(bridge: Arc<Bridge>, pool_size: usize) -> Result<Self, PgPartitionError> {
        check_context(&bridge, pool_size)?;
        Ok(Self { bridge })
    }

    /// Creates the tables if they are missing. Idempotent; shared with the registry.
    ///
    /// # Errors
    /// `partition_pg.prepare_failed` when the DDL cannot be applied or the bridge refuses.
    pub fn prepare(&self) -> Result<(), PgPartitionError> {
        prepare_all(&self.bridge)
    }

    /// Every range granted so far, oldest first, or the failure.
    ///
    /// # Errors
    /// [`PartitionError::Store`] when the database or the bridge fails.
    pub fn try_grants(&self) -> Result<Vec<RangeGrant>, PartitionError> {
        self.bridge
            .run(GRANTS_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn
                        .query(
                            "SELECT lease_id, series_key, node, first_number::text, \
                                    last_number::text, granted_at::text \
                             FROM spot_range_grant ORDER BY lease_id",
                            &[],
                        )
                        .await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        let lease_id =
                            u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
                        out.push(RangeGrant {
                            lease: RangeLease {
                                series: SeriesKey::new(row.get::<_, &str>(1))
                                    .map_err(|_| OpError::Decode)?,
                                node: NodeId::new(row.get::<_, &str>(2))
                                    .map_err(|_| OpError::Decode)?,
                                from: decode_u64(row.get(3))?,
                                to: decode_u64(row.get(4))?,
                                lease_id: LeaseId::new(lease_id),
                            },
                            granted_at: decode_instant(row.get(5))?,
                        });
                    }
                    Ok::<_, OpError>(out)
                })
            })
            .map_err(map)
    }
}

impl RangeAllocator for PgRangeAllocator {
    fn lease(
        &mut self,
        series: &SeriesKey,
        node: &NodeId,
        size: u64,
        at: OffsetDateTime,
    ) -> Result<RangeLease, PartitionError> {
        if size == 0 {
            return Err(PartitionError::InvalidArgument {
                reason: "a leased range holds at least one number".to_owned(),
            });
        }
        let (series, node) = (series.clone(), node.clone());
        let at = encode_instant(at);
        self.bridge
            .run(LEASE_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let key = series.as_str();
                    // The first use creates the row; a concurrent first use waits on the
                    // winner's commit here and then does nothing.
                    tx.execute(
                        "INSERT INTO spot_range_series (series_key, next_free) VALUES ($1, 1) \
                         ON CONFLICT (series_key) DO NOTHING",
                        &[&key],
                    )
                    .await?;
                    // The row lock serialises every grant of this series, in any process.
                    let cursor_text: String = tx
                        .query_one(
                            "SELECT next_free::text FROM spot_range_series \
                             WHERE series_key = $1 FOR UPDATE",
                            &[&key],
                        )
                        .await?
                        .get(0);
                    let cursor = decode_u128(&cursor_text)?;
                    if cursor > u128::from(u64::MAX) {
                        return Err(OpError::Refused(PartitionError::RangeExhausted {
                            series: key.to_owned(),
                            node: node.to_string(),
                        }));
                    }
                    // Checked above: the cursor fits a u64.
                    let from = u64::try_from(cursor).map_err(|_| OpError::Decode)?;
                    let to = from.checked_add(size - 1).ok_or_else(|| {
                        OpError::Refused(PartitionError::InvalidRange {
                            reason: "the range overflows the number space".to_owned(),
                        })
                    })?;
                    if to == u64::MAX {
                        return Err(OpError::Refused(PartitionError::InvalidRange {
                            reason: "the range would reach the largest number".to_owned(),
                        }));
                    }
                    let next = (u128::from(to) + 1).to_string();
                    tx.execute(
                        "UPDATE spot_range_series SET next_free = $2::text::numeric \
                         WHERE series_key = $1",
                        &[&key, &next],
                    )
                    .await?;
                    let (first, last) = (from.to_string(), to.to_string());
                    let node_text = node.as_str();
                    let lease_id: i64 = tx
                        .query_one(
                            "INSERT INTO spot_range_grant \
                             (series_key, node, first_number, last_number, granted_at) \
                             VALUES ($1, $2, $3::text::numeric, $4::text::numeric, \
                                     $5::text::numeric) RETURNING lease_id",
                            &[&key, &node_text, &first, &last, &at],
                        )
                        .await?
                        .get(0);
                    tx.commit().await?;
                    let lease_id = u64::try_from(lease_id).map_err(|_| OpError::Decode)?;
                    Ok::<_, OpError>(RangeLease {
                        series,
                        from,
                        to,
                        node,
                        lease_id: LeaseId::new(lease_id),
                    })
                })
            })
            .map_err(map)
    }

    /// Every range granted so far, oldest first.
    ///
    /// The trait cannot report a failure. When the read fails this returns an **empty list**, and
    /// only a caller reading it as "nothing was ever granted" could be misled: the allocator
    /// itself never uses this list to decide what to grant (the cursor in the database does), so
    /// a failed read can never cause a number to be reissued. Use
    /// [`PgRangeAllocator::try_grants`] to get the error.
    fn grants(&self) -> Vec<RangeGrant> {
        self.try_grants().unwrap_or_default()
    }
}
