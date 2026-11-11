//! How much, how often, and of which series a node may be granted.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::Duration;

/// The default most numbers one range may hold.
pub const DEFAULT_MAX_SIZE: u64 = 10_000;

/// The default shortest time between two grants of one series to one node: 60 seconds.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::seconds(60);

/// The default most distinct series the service tracks: 256.
///
/// A **capacity** limit, not a policy statement: it bounds the memory the service's cursors,
/// last-grant times and idempotency keys can take however many series a node invents. The
/// value is provisional until the hub wiring task decides how many series a hub serves.
pub const DEFAULT_MAX_SERIES: usize = 256;

/// The default most requests remembered per node for idempotent re-requests: 512.
pub const DEFAULT_MAX_REMEMBERED_REQUESTS: usize = 512;

/// The default window over which refusals of one node, series and reason are counted: 60
/// seconds.
pub const DEFAULT_REFUSAL_WINDOW: Duration = Duration::seconds(60);

/// The size, rate and entitlement limits of grants.
///
/// The hub cannot see how much of a lease a node used, and a node-sent usage figure is not
/// trusted, so the limits are these: which series a node **may** lease at all
/// ([`GrantPolicy::with_node_series`]), the most distinct series the service tracks, a
/// **maximum size** per range, and a **minimum interval** between two grants of one series to
/// one node (measured on the instants the caller passes). Together they bound how fast a
/// compromised node can drain a series, and how much memory a node's asking can cost the hub.
///
/// The entitlement starts **empty**: a policy that names no series for a node grants that node
/// nothing, so no existing path silently widens when this crate is upgraded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantPolicy {
    default_max: u64,
    per_series: BTreeMap<String, u64>,
    min_interval: Duration,
    /// Per node, the series it may lease. Empty means nothing.
    entitlements: BTreeMap<String, BTreeSet<String>>,
    max_series: usize,
    max_remembered: usize,
    refusal_window: Duration,
}

impl Default for GrantPolicy {
    fn default() -> Self {
        Self {
            default_max: DEFAULT_MAX_SIZE,
            per_series: BTreeMap::new(),
            min_interval: DEFAULT_MIN_INTERVAL,
            entitlements: BTreeMap::new(),
            max_series: DEFAULT_MAX_SERIES,
            max_remembered: DEFAULT_MAX_REMEMBERED_REQUESTS,
            refusal_window: DEFAULT_REFUSAL_WINDOW,
        }
    }
}

impl GrantPolicy {
    /// The default policy: 10,000 numbers per range, one grant a minute, 256 distinct series,
    /// 512 remembered requests per node, a 60-second refusal window — and **no** node entitled
    /// to any series until one is named.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same policy with another default maximum size.
    #[must_use]
    pub fn with_default_max(mut self, max: u64) -> Self {
        self.default_max = max;
        self
    }

    /// The same policy with a maximum size for one series.
    #[must_use]
    pub fn with_series_max(mut self, series: &SeriesKey, max: u64) -> Self {
        self.per_series.insert(series.as_str().to_owned(), max);
        self
    }

    /// The same policy with another minimum interval between grants.
    #[must_use]
    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    /// The same policy with `series` added to the series `node` may lease.
    ///
    /// Only series named here are granted (see [`GrantPolicy::entitles`]); the hub fills this
    /// from whatever owns the node-to-series assignment.
    #[must_use]
    pub fn with_node_series(mut self, node: &NodeId, series: &SeriesKey) -> Self {
        self.entitlements
            .entry(node.as_str().to_owned())
            .or_default()
            .insert(series.as_str().to_owned());
        self
    }

    /// The same policy with another cap on the distinct series the service tracks.
    #[must_use]
    pub fn with_max_series(mut self, max: usize) -> Self {
        self.max_series = max;
        self
    }

    /// The same policy with another cap on the requests remembered per node.
    #[must_use]
    pub fn with_max_remembered_requests(mut self, max: usize) -> Self {
        self.max_remembered = max;
        self
    }

    /// The same policy with another window for counting a node's refusals.
    #[must_use]
    pub fn with_refusal_window(mut self, window: Duration) -> Self {
        self.refusal_window = window;
        self
    }

    /// The most numbers one range of `series` may hold.
    #[must_use]
    pub fn max_size(&self, series: &SeriesKey) -> u64 {
        self.per_series
            .get(series.as_str())
            .copied()
            .unwrap_or(self.default_max)
    }

    /// The shortest time between two grants of one series to one node.
    #[must_use]
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Whether `node` may lease `series`. False unless the policy names the pair.
    #[must_use]
    pub fn entitles(&self, node: &NodeId, series: &SeriesKey) -> bool {
        self.entitlements
            .get(node.as_str())
            .is_some_and(|entitled| entitled.contains(series.as_str()))
    }

    /// The series entitled to `node`, in order (empty unless the policy names some).
    #[must_use]
    pub fn entitled_series(&self, node: &NodeId) -> Vec<&str> {
        self.entitlements
            .get(node.as_str())
            .map(|series| series.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// The most distinct series the service tracks.
    #[must_use]
    pub fn max_series(&self) -> usize {
        self.max_series
    }

    /// The most requests remembered per node for idempotent re-requests.
    #[must_use]
    pub fn max_remembered_requests(&self) -> usize {
        self.max_remembered
    }

    /// The window over which refusals of one node, series and reason are counted.
    #[must_use]
    pub fn refusal_window(&self) -> Duration {
        self.refusal_window
    }
}
