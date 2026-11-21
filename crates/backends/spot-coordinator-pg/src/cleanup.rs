//! Sweeping expired rows: a job's work, not a background timer.
//!
//! [`cleanup`] deletes leases past their expiry, rate buckets that have fully refilled (a full
//! bucket is the same as no row), and node rows not seen within the presence window. A scheduled
//! job (WP-PLT-06) calls it; the coordinator itself never runs a timer. It is safe to run from
//! several nodes at once.

use std::time::Duration;

use deadpool_postgres::Pool;
use spot_coordinator::{Error, Result};

/// What one cleanup removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Expired lease rows deleted.
    pub leases: u64,
    /// Fully refilled rate buckets deleted.
    pub rate_buckets: u64,
    /// Node rows past the presence window deleted.
    pub nodes: u64,
}

/// Deletes expired leases, refilled rate buckets and dead node rows. Nodes are dead when their
/// last heartbeat is older than `node_window`.
///
/// # Errors
///
/// `coordinator.unavailable` when the database or its pool fails.
pub async fn cleanup(pool: &Pool, node_window: Duration) -> Result<CleanupReport> {
    let client = pool
        .get()
        .await
        .map_err(|e| crate::error::pool("cleanup", &e))?;
    let leases = client
        .execute(EXPIRED_LEASES_SQL, &[])
        .await
        .map_err(|e| crate::error::pg("cleanup leases", &e))?;
    let rate_buckets = client
        .execute(REFILLED_RATE_SQL, &[])
        .await
        .map_err(|e| crate::error::pg("cleanup rate buckets", &e))?;
    let window = i64::try_from(node_window.as_micros())
        .map_err(|_| Error::invalid_argument("the presence window is too large"))?;
    let nodes = client
        .execute(DEAD_NODES_SQL, &[&window])
        .await
        .map_err(|e| crate::error::pg("cleanup nodes", &e))?;
    Ok(CleanupReport {
        leases,
        rate_buckets,
        nodes,
    })
}

const EXPIRED_LEASES_SQL: &str = "DELETE FROM spot_coord_lease WHERE expires_at <= now()";

const REFILLED_RATE_SQL: &str = "DELETE FROM spot_coord_rate WHERE window_start <= now()";

const DEAD_NODES_SQL: &str = "\
DELETE FROM spot_coord_node \
WHERE seen_at <= now() - $1::bigint * interval '1 microsecond'";
