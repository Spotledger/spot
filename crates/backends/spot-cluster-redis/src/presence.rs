//! Node presence in Redis: keys with a TTL, for the in-process coordinator's second-node
//! detection.
//!
//! [`RedisPresence`] implements [`NodePresence`], so it can be handed to
//! [`InProcessCoordinator::start`](spot_coordinator::InProcessCoordinator::start) in place of the
//! in-memory fake: a node that starts a second process finds the first one's key and refuses to
//! grant leases. Each registration writes `{prefix}:coord:node:{node}` with a TTL, so a crashed
//! node disappears without anyone cleaning up, and a clean shutdown deletes it immediately.

use std::time::Duration;

use redis::aio::ConnectionManager;
use spot_coordinator::coordinator::BoxFuture;
use spot_coordinator::{NodeId, NodePresence, Registration, Result as CoordResult};

use crate::connection::RedisConnection;

/// How long a node is considered present after its last registration, by default.
pub const DEFAULT_NODE_WINDOW: Duration = Duration::from_secs(15);

/// Presence in Redis: one TTL key per running node.
#[derive(Clone, Debug)]
pub struct RedisPresence {
    connection: RedisConnection,
    window: Duration,
}

impl RedisPresence {
    /// Presence over `connection`, where a node's key lives for `window`.
    #[must_use]
    pub fn new(connection: RedisConnection, window: Duration) -> Self {
        Self { connection, window }
    }

    /// Presence over `connection` with the default window.
    #[must_use]
    pub fn with_default(connection: RedisConnection) -> Self {
        Self::new(connection, DEFAULT_NODE_WINDOW)
    }

    /// The window three heartbeats of `interval` implies, matching the database backend.
    #[must_use]
    pub fn window_for(interval: Duration) -> Duration {
        interval.saturating_mul(3)
    }

    /// The node key of `node`.
    #[must_use]
    pub fn key(&self, node: &NodeId) -> String {
        self.connection.keyspace().node(node.as_str())
    }

    fn window_millis(&self) -> u64 {
        u64::try_from(self.window.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }
}

impl NodePresence for RedisPresence {
    fn register<'a>(&'a self, node: &'a NodeId) -> BoxFuture<'a, CoordResult<Registration>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            redis::cmd("SET")
                .arg(self.key(node))
                .arg("1")
                .arg("PX")
                .arg(self.window_millis())
                .query_async::<String>(&mut manager)
                .await
                .map_err(|error| {
                    spot_coordinator::Error::new(
                        crate::error::UNAVAILABLE,
                        format!("registering node {}: {error}", node.as_str()),
                    )
                })?;
            let guard = PresenceGuard {
                manager: self.connection.manager(),
                key: self.key(node),
            };
            Ok(Registration::new(guard))
        })
    }

    fn others<'a>(&'a self, me: &'a NodeId) -> BoxFuture<'a, CoordResult<Vec<NodeId>>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let prefix = format!("{}:coord:node:", self.connection.keyspace().prefix());
            let pattern = self.connection.keyspace().node_pattern();
            let mut cursor = "0".to_owned();
            let mut others: Vec<NodeId> = Vec::new();
            loop {
                let (next, keys): (String, Vec<String>) = redis::cmd("SCAN")
                    .arg(&cursor)
                    .arg("MATCH")
                    .arg(&pattern)
                    .arg("COUNT")
                    .arg(100)
                    .query_async(&mut manager)
                    .await
                    .map_err(|error| {
                        spot_coordinator::Error::new(
                            crate::error::UNAVAILABLE,
                            format!("scanning presence: {error}"),
                        )
                    })?;
                for key in keys {
                    let Some(name) = key.strip_prefix(&prefix) else {
                        continue;
                    };
                    if name == me.as_str() {
                        continue;
                    }
                    let node = NodeId::new(name).map_err(|error| {
                        spot_coordinator::Error::new(crate::error::PROTOCOL, error.to_string())
                    })?;
                    others.push(node);
                }
                if next == "0" {
                    break;
                }
                cursor = next;
            }
            others.sort();
            others.dedup();
            Ok(others)
        })
    }
}

/// Deletes the node's key when the registration is dropped, best effort.
struct PresenceGuard {
    manager: ConnectionManager,
    key: String,
}

impl std::fmt::Debug for PresenceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresenceGuard")
            .field("key", &self.key)
            .finish()
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        // No runtime (or one shutting down): the TTL withdraws the node on its own.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut manager = self.manager.clone();
        let key = std::mem::take(&mut self.key);
        handle.spawn(async move {
            let _ = redis::cmd("DEL")
                .arg(key)
                .query_async::<i64>(&mut manager)
                .await;
        });
    }
}
