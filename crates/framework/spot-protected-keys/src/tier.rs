//! The permission-snapshot tier that bounds a key lease (ADR-070 rule 12.13 b and d).
//!
//! The tier comes from the signed permission snapshot the hub publishes (WP-IAM-09); this
//! crate only defines the seam. A group's key lease equals its tier's bound.

use crate::ids::{GroupName, NodeId};
use crate::time::Duration;
use std::collections::BTreeMap;

/// The three risk tiers of ADR-070 rule 12.13 d.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TierName {
    /// Payments, approvals, administrator actions and `reveal`.
    One,
    /// Reads of protected groups.
    Two,
    /// Ordinary work.
    Three,
}

impl TierName {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::One => "tier1",
            Self::Two => "tier2",
            Self::Three => "tier3",
        }
    }
}

/// A tier and the bound its key lease runs for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    name: TierName,
    bound: Duration,
}

impl Tier {
    /// A tier with `bound`.
    #[must_use]
    pub fn new(name: TierName, bound: Duration) -> Self {
        Self { name, bound }
    }

    /// Tier 1: one hour (within the 15-minute to 1-hour proposal of the research report).
    #[must_use]
    pub fn one() -> Self {
        Self::new(TierName::One, Duration::from_hours(1))
    }

    /// Tier 2: twelve hours.
    #[must_use]
    pub fn two() -> Self {
        Self::new(TierName::Two, Duration::from_hours(12))
    }

    /// Tier 3: the 24-hour default.
    #[must_use]
    pub fn three() -> Self {
        Self::new(TierName::Three, Duration::from_hours(24))
    }

    /// The tier name.
    #[must_use]
    pub fn name(&self) -> TierName {
        self.name
    }

    /// The lease bound.
    #[must_use]
    pub fn bound(&self) -> Duration {
        self.bound
    }
}

/// The permission-snapshot seam (from WP-IAM-09). A node has a tier for a group, or none.
pub trait PermissionTier: Send + Sync {
    /// The tier a node has for a group, or `None` when it has no access.
    fn tier(&self, node: &NodeId, group: &GroupName) -> Option<Tier>;
}

/// A plain, in-memory tier table, for tests and single-process use.
#[derive(Clone, Debug, Default)]
pub struct TierTable {
    grants: BTreeMap<(NodeId, GroupName), Tier>,
}

impl TierTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants `node` the tier `tier` for `group`.
    pub fn grant(&mut self, node: NodeId, group: GroupName, tier: Tier) {
        self.grants.insert((node, group), tier);
    }

    /// Removes a grant.
    pub fn revoke(&mut self, node: &NodeId, group: &GroupName) {
        self.grants.remove(&(node.clone(), group.clone()));
    }

    /// How many grants the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

impl PermissionTier for TierTable {
    fn tier(&self, node: &NodeId, group: &GroupName) -> Option<Tier> {
        self.grants.get(&(node.clone(), group.clone())).copied()
    }
}
