//! A health probe for `spot-fleet-health`: is the coordination/cache server reachable?
//!
//! The probe never fails: it reports `reachable: false` with a plain-language reason, so the fleet
//! health surface can render a degraded backend without a panic or an error code.

use crate::connection::RedisConnection;

/// The outcome of one probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthReport {
    /// Whether the server answered `PING`.
    pub reachable: bool,
    /// The key prefix the plugin is using (so two clusters on one server are distinguishable).
    pub keyspace: String,
    /// A short reason, empty when reachable.
    pub detail: String,
}

impl HealthReport {
    /// Whether the backend is usable.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.reachable
    }
}

/// Probes `connection` with a `PING`.
pub async fn probe(connection: &RedisConnection) -> HealthReport {
    match connection.ping().await {
        Ok(()) => HealthReport {
            reachable: true,
            keyspace: connection.keyspace().prefix().to_owned(),
            detail: String::new(),
        },
        Err(error) => HealthReport {
            reachable: false,
            keyspace: connection.keyspace().prefix().to_owned(),
            detail: error.to_string(),
        },
    }
}
