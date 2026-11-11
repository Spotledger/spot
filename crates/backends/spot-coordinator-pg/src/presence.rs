//! Node presence in the database: second-node detection for a standalone profile on a shared
//! database.
//!
//! [`PgPresence`] implements [`NodePresence`]. A node `register`s itself (an upsert of its
//! heartbeat row), the host loop calls [`PgPresence::heartbeat`] every heartbeat interval, and
//! [`PgPresence::others`] lists the nodes seen within a window (three heartbeat intervals by
//! default). Started with this presence, the in-process coordinator refuses to start beside a
//! live node in the same database, which is how a standalone profile detects a second node.
//!
//! Dropping the [`Registration`] deletes the node's row, best effort, through the runtime; if it
//! cannot, the row ages out of the window instead.

use std::time::Duration;

use deadpool_postgres::Pool;
use spot_coordinator::coordinator::BoxFuture;
use spot_coordinator::presence::{NodePresence, Registration};
use spot_coordinator::{Error, NodeId, Result};

/// How many heartbeat intervals a node is considered live for after its last heartbeat.
pub const NODE_WINDOW_INTERVALS: u32 = 3;

/// The default heartbeat interval.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(5);

/// The `spot_coord_node`-backed presence.
#[derive(Clone)]
pub struct PgPresence {
    pool: Pool,
    window: Duration,
}

impl PgPresence {
    /// A presence over `pool`, considering a node live for `window` after its last heartbeat.
    #[must_use]
    pub fn new(pool: Pool, window: Duration) -> PgPresence {
        PgPresence { pool, window }
    }

    /// The window from a heartbeat interval: [`NODE_WINDOW_INTERVALS`] intervals.
    #[must_use]
    pub fn window_for(heartbeat: Duration) -> Duration {
        heartbeat.saturating_mul(NODE_WINDOW_INTERVALS)
    }

    /// Records that `node` is running now.
    ///
    /// # Errors
    ///
    /// `coordinator.unavailable` when the database call fails.
    pub async fn heartbeat(&self, node: &NodeId) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| crate::error::pool("heartbeat", &e))?;
        client
            .execute(HEARTBEAT_SQL, &[&node.as_str(), &"spot"])
            .await
            .map_err(|e| crate::error::pg("heartbeat", &e))?;
        Ok(())
    }

    /// Whether `node` is seen now.
    ///
    /// # Errors
    ///
    /// `coordinator.unavailable` when the database call fails.
    pub async fn seen(&self, node: &NodeId) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| crate::error::pool("seen", &e))?;
        let row = client
            .query_opt(SEEN_SQL, &[&node.as_str(), &self.window_micros()?])
            .await
            .map_err(|e| crate::error::pg("seen", &e))?;
        Ok(row.is_some())
    }

    fn window_micros(&self) -> Result<i64> {
        i64::try_from(self.window.as_micros())
            .map_err(|_| Error::invalid_argument("the presence window is too large"))
    }
}

impl NodePresence for PgPresence {
    fn register<'a>(&'a self, node: &'a NodeId) -> BoxFuture<'a, Result<Registration>> {
        Box::pin(async move {
            self.heartbeat(node).await?;
            Ok(Registration::new(Withdraw {
                pool: self.pool.clone(),
                node: node.clone(),
            }))
        })
    }

    fn others<'a>(&'a self, me: &'a NodeId) -> BoxFuture<'a, Result<Vec<NodeId>>> {
        Box::pin(async move {
            let client = self
                .pool
                .get()
                .await
                .map_err(|e| crate::error::pool("others", &e))?;
            let rows = client
                .query(OTHERS_SQL, &[&me.as_str(), &self.window_micros()?])
                .await
                .map_err(|e| crate::error::pg("others", &e))?;
            let mut nodes = Vec::with_capacity(rows.len());
            for row in rows {
                let id: String = row.get(0);
                nodes.push(NodeId::new(id).map_err(|e| {
                    Error::new(
                        crate::error::UNAVAILABLE,
                        format!("a presence row holds an invalid node id: {e}"),
                    )
                })?);
            }
            Ok(nodes)
        })
    }
}

/// Deletes the node's row when the registration is dropped, through the current runtime when
/// there is one; otherwise the row ages out of the window.
struct Withdraw {
    pool: Pool,
    node: NodeId,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = self.pool.clone();
        let node = self.node.clone();
        handle.spawn(async move {
            if let Ok(client) = pool.get().await {
                let _ = client
                    .execute(
                        "DELETE FROM spot_coord_node WHERE node_id = $1",
                        &[&node.as_str()],
                    )
                    .await;
            }
        });
    }
}

const HEARTBEAT_SQL: &str = "\
INSERT INTO spot_coord_node (node_id, seen_at, info) VALUES ($1, now(), $2) \
ON CONFLICT (node_id) DO UPDATE SET seen_at = now()";

const SEEN_SQL: &str = "\
SELECT 1 FROM spot_coord_node \
WHERE node_id = $1 AND seen_at > now() - $2::bigint * interval '1 microsecond'";

const OTHERS_SQL: &str = "\
SELECT node_id FROM spot_coord_node \
WHERE node_id <> $1 AND seen_at > now() - $2::bigint * interval '1 microsecond' \
ORDER BY node_id";
