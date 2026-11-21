//! The sync stamp on every submitted `record` document (ADR-065 section 5).
//!
//! The stamp's `stamped_at_millis` is the node's **wall-clock** time; it is the only wall-clock
//! value in this crate and it is passed in, never read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::tracker::{FreshnessSettings, FreshnessTracker};

/// A recorded supervisor override of a `block` policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampOverride {
    /// Who overrode.
    pub actor: String,
    /// The stale classes that were overridden.
    pub classes: Vec<String>,
}

/// A recorded confirmation of a `warn` policy warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningConfirmed {
    /// Who confirmed.
    pub actor: String,
}

/// What a document was made under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStamp {
    /// The node that posted the document.
    pub node: String,
    /// The node's wall-clock time at posting, milliseconds since the Unix epoch.
    pub stamped_at_millis: i64,
    /// The node's estimate of (hub clock minus node clock), milliseconds.
    pub hub_offset_ms: i64,
    /// The node's applied prefix when it posted.
    pub applied_prefix: u64,
    /// Per class, the horizon position the node last confirmed itself current at.
    pub class_horizons_confirmed: BTreeMap<String, u64>,
    /// Classes that were stale when posting.
    pub stale: Vec<String>,
    /// Effective metadata version the document was made under.
    pub metadata_version: u64,
    /// Software version of the node.
    pub software_version: String,
    /// Set when a supervisor overrode a block.
    #[serde(rename = "override", default, skip_serializing_if = "Option::is_none")]
    pub override_: Option<StampOverride>,
    /// Set when the user confirmed a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning_confirmed: Option<WarningConfirmed>,
}

impl SyncStamp {
    /// The hub-timeline time of the stamp **as the node claims it**: `stamped_at + hub_offset`,
    /// saturating. A pure function of the claim, so a hostile node can move it anywhere.
    ///
    /// Never use it to look up mandatory events or to build a posting time; use
    /// [`HubPostedAt::from_clamped`](crate::HubPostedAt::from_clamped). It exists for the audit
    /// note that compares the claim with the clamp.
    #[must_use]
    pub fn claimed_hub_time(&self) -> i64 {
        self.stamped_at_millis.saturating_add(self.hub_offset_ms)
    }
}

/// Everything a stamp is built from at submission.
#[derive(Debug, Clone)]
pub struct StampInput<'a> {
    /// The node id.
    pub node: &'a str,
    /// Node wall-clock now, milliseconds since the epoch.
    pub stamped_at_millis: i64,
    /// Current offset estimate (hub minus node), milliseconds.
    pub hub_offset_ms: i64,
    /// The node's applied prefix.
    pub applied_prefix: u64,
    /// Every freshness class the node tracks.
    pub classes: &'a [String],
    /// Node monotonic now, milliseconds.
    pub now_monotonic_millis: u64,
    /// Effective metadata version.
    pub metadata_version: u64,
    /// Node software version.
    pub software_version: &'a str,
}

/// Build the stamp for a document being submitted.
#[must_use]
pub fn build_stamp(
    input: &StampInput<'_>,
    tracker: &FreshnessTracker,
    settings: &FreshnessSettings,
    override_: Option<StampOverride>,
    warning_confirmed: Option<WarningConfirmed>,
) -> SyncStamp {
    SyncStamp {
        node: input.node.to_owned(),
        stamped_at_millis: input.stamped_at_millis,
        hub_offset_ms: input.hub_offset_ms,
        applied_prefix: input.applied_prefix,
        class_horizons_confirmed: tracker.confirmed_horizons(),
        stale: tracker.stale_classes(input.classes, input.now_monotonic_millis, settings),
        metadata_version: input.metadata_version,
        software_version: input.software_version.to_owned(),
        override_,
        warning_confirmed,
    }
}
