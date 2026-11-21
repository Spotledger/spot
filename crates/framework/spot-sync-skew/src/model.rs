//! Handshake report types.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// An event topic (the event type name, ADR-056).
pub type Topic = String;
/// A node identifier.
pub type NodeId = String;
/// An event schema major (ADR-056).
pub type Major = u32;

/// A software version, ordered major, minor, patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SoftwareVersion {
    /// Major release number.
    pub major: u32,
    /// Minor release number.
    pub minor: u32,
    /// Patch number (never counts as a release for skew).
    pub patch: u32,
}

impl SoftwareVersion {
    /// Build a version.
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// What one side reports in the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    /// The software version.
    pub software_version: SoftwareVersion,
    /// The metadata version.
    pub metadata_version: u32,
    /// The migration epoch.
    pub migration_epoch: u64,
    /// Whether that epoch is verified (WP-MIG-12).
    pub epoch_verified: bool,
    /// True while this side runs a migration.
    pub mid_migration: bool,
    /// The event schema majors this side can decode, per topic.
    pub decodable_majors: BTreeMap<Topic, BTreeSet<Major>>,
}

impl NodeReport {
    /// Whether this side can decode `major` of `topic`.
    #[must_use]
    pub fn can_decode(&self, topic: &str, major: Major) -> bool {
        self.decodable_majors
            .get(topic)
            .is_some_and(|set| set.contains(&major))
    }
}

/// The tenant settings this library reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// How many releases a node may be behind the hub (ADR-066 default 1).
    pub allowed_release_skew: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            allowed_release_skew: 1,
        }
    }
}

/// A node's lifecycle status in the hub's node state (ADR-065), reduced to what retirement reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Syncing normally; blocks retirement until it can decode the new major.
    Active,
    /// Suspended; never blocks retirement.
    Suspended,
    /// Further behind than the hub can hold records for (its hold set is full, or an entry's major
    /// is retired): only the upgrade channel is open.
    TooOld,
    /// Any other status (bootstrapping, retired and the like); does not block.
    Other,
}

/// One node as the hub sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// The node.
    pub node: NodeId,
    /// Its status.
    pub status: NodeStatus,
    /// Its last handshake report.
    pub report: NodeReport,
}
