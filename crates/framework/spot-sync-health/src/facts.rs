//! What the caller knows about a node, handed in as plain data.

use spot_conditions::Millis;
use spot_sync_acks::NodeReportRow;

use crate::error::HealthError;
use crate::model::NodeStatus;

/// One node's facts for the view. All times are hub time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHealthFacts {
    /// The node id, as the transport authenticated it.
    pub node: String,
    /// When the hub last heard from the node.
    pub last_contact: Millis,
    /// The applied prefix the node last reported.
    pub applied_prefix: u64,
    /// The newest position the hub's own stream holds.
    pub head: u64,
    /// Positions per minute the node has recently applied; zero when unknown.
    pub apply_rate_per_minute: u64,
    /// The software version tag the node reported.
    pub version: String,
    /// The freshness and skew verdict, reduced by the caller.
    pub status: NodeStatus,
    /// The time the facts are true as of.
    pub as_of: Millis,
}

impl NodeHealthFacts {
    /// Facts from the ledger's as-of row, plus what the row does not carry.
    ///
    /// # Errors
    ///
    /// [`HealthError::NegativeContactTime`] when the row's contact time is before the epoch.
    pub fn from_row(
        row: &NodeReportRow,
        head: u64,
        apply_rate_per_minute: u64,
        status: NodeStatus,
        as_of: Millis,
    ) -> Result<Self, HealthError> {
        let last_contact = u64::try_from(row.last_contact_millis).map_err(|_| {
            HealthError::NegativeContactTime {
                node: row.node.clone(),
            }
        })?;
        Ok(Self {
            node: row.node.clone(),
            last_contact: Millis::new(last_contact),
            applied_prefix: row.applied_prefix,
            head,
            apply_rate_per_minute,
            version: row.version.clone(),
            status,
            as_of,
        })
    }

    /// Positions the node is behind the hub's head (never negative).
    #[must_use]
    pub fn lag_positions(&self) -> u64 {
        self.head.saturating_sub(self.applied_prefix)
    }

    /// Estimated milliseconds to catch up, or `None` when the rate is unknown.
    #[must_use]
    pub fn lag_estimate(&self) -> Option<Millis> {
        if self.apply_rate_per_minute == 0 {
            return None;
        }
        let millis =
            u128::from(self.lag_positions()) * 60_000 / u128::from(self.apply_rate_per_minute);
        Some(Millis::new(u64::try_from(millis).unwrap_or(u64::MAX)))
    }
}
