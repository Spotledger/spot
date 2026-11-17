//! `services.sync_hub.catchup.*`: the thresholds and bounds, with documented ranges. Unknown keys
//! are refused.

use serde::Deserialize;
use spot_sync_bootstrap::AdmissionSettings;
use spot_sync_catchup::{Settings, Share};

use crate::error::HostError;

/// The catch-up configuration of a hub.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct CatchupConfig {
    /// The largest gap still replayed in position order, in in-scope events (1 to 10,000,000;
    /// default 5,000). Above it the backlog is coalesced.
    pub catchup_threshold_events: u64,
    /// The share, in percent, of a full scoped snapshot above which a coalesced plan is not worth
    /// it and a snapshot bootstrap is chosen (1 to 100; default 40).
    pub snapshot_share_percent: u8,
    /// The most records in one planned bundle (1 to 10,000; default 500).
    pub bundle_records: usize,
    /// The most bootstraps running at once per tenant (1 to 64; default 4): 100 nodes returning
    /// together do not read 100 snapshots at once.
    pub max_parallel_bootstraps: usize,
    /// The least a refused node waits, in milliseconds (default 2,000).
    pub retry_base_ms: u64,
    /// The jitter window on top of it, in milliseconds (default 3,000).
    pub retry_jitter_ms: u64,
    /// One chunk token is added per node every this many milliseconds (default 100).
    pub chunk_interval_ms: u64,
    /// The most chunk tokens a node can hold (default 4).
    pub chunk_burst: u32,
    /// How many releases a node may be behind the hub (default 1).
    pub allowed_release_skew: u32,
}

impl Default for CatchupConfig {
    fn default() -> Self {
        Self {
            catchup_threshold_events: 5_000,
            snapshot_share_percent: 40,
            bundle_records: 500,
            max_parallel_bootstraps: 4,
            retry_base_ms: 2_000,
            retry_jitter_ms: 3_000,
            chunk_interval_ms: 100,
            chunk_burst: 4,
            allowed_release_skew: 1,
        }
    }
}

impl CatchupConfig {
    /// Checks the bounds.
    ///
    /// # Errors
    /// [`HostError::ConfigInvalid`] naming the key.
    pub fn validate(&self) -> Result<(), HostError> {
        if !(1..=10_000_000).contains(&self.catchup_threshold_events) {
            return Err(HostError::ConfigInvalid(
                "catchup_threshold_events is outside 1..=10000000",
            ));
        }
        if !(1..=100).contains(&self.snapshot_share_percent) {
            return Err(HostError::ConfigInvalid(
                "snapshot_share_percent is outside 1..=100",
            ));
        }
        if !(1..=10_000).contains(&self.bundle_records) {
            return Err(HostError::ConfigInvalid(
                "bundle_records is outside 1..=10000",
            ));
        }
        if !(1..=64).contains(&self.max_parallel_bootstraps) {
            return Err(HostError::ConfigInvalid(
                "max_parallel_bootstraps is outside 1..=64",
            ));
        }
        if self.chunk_interval_ms == 0 || self.chunk_burst == 0 {
            return Err(HostError::ConfigInvalid(
                "chunk_interval_ms and chunk_burst must be positive",
            ));
        }
        Ok(())
    }

    /// The planner's settings.
    ///
    /// # Errors
    /// [`HostError`] for an out-of-bounds value.
    pub fn planner(&self) -> Result<Settings, HostError> {
        self.validate()?;
        Ok(Settings {
            incremental_limit: self.catchup_threshold_events,
            snapshot_fraction: Share::from_percent(self.snapshot_share_percent)
                .map_err(|e| HostError::Planner(e.code()))?,
            bundle_records: self.bundle_records,
        })
    }

    /// The bootstrap admission's settings.
    #[must_use]
    pub fn admission(&self) -> AdmissionSettings {
        AdmissionSettings {
            max_concurrent_bootstraps: self.max_parallel_bootstraps,
            retry_base_ms: self.retry_base_ms,
            retry_jitter_ms: self.retry_jitter_ms,
            chunk_interval_ms: self.chunk_interval_ms,
            chunk_burst: self.chunk_burst,
        }
    }
}
