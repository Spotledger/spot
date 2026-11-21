//! The as-of row the Admin Center shows for a node that has not applied an event.

use serde::{Deserialize, Serialize};
use spot_sync_obligations::Level;

/// One node that has not applied a mandatory event, as of its last report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReportRow {
    /// The node id.
    pub node: String,
    /// The tenant whose stream the row is about. A node serving several tenants has one row
    /// per tenant. Rows serialised before the ledger was keyed by tenant read as the empty
    /// tenant.
    #[serde(default)]
    pub tenant: String,
    /// When the hub last heard from the node, milliseconds in hub time: the row is true as of
    /// then, not as of now.
    pub last_contact_millis: i64,
    /// The applied prefix the node last reported.
    pub applied_prefix: u64,
    /// True when the event is at or below the prefix but held (so still not applied).
    pub held: bool,
    /// The software version tag the node reported.
    pub version: String,
    /// The node's escalation level at the last evaluation.
    pub level: Level,
}
