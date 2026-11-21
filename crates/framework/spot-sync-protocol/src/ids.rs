//! Node identity and protocol versions.
//!
//! A **node** is a runtime with its own database and its own stream: the hub (head office) or an
//! edge (a branch, plant or warehouse that keeps working without the hub) (ADR-062 section 3). A
//! [`NodeId`] is the identity an edge presents, and the name a hub reports; the transport binding
//! (WP-EDG-02.T2) maps a client certificate's subject onto one through its node directory.
//!
//! [`ProtocolRange`] is the version-negotiation field of [`Hello`](crate::Hello): an edge says
//! which protocol versions it speaks, and the hub answers with the highest one they share. The
//! range is additive by design, so a newer peer can still talk to an older one.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A node's identity: the runtime that owns a stream.
///
/// Written like a source id (ADR-056): a lower-case host name, optionally followed by `#` and a
/// storage-name partition (`edge-khi#branch_01`). A hub is usually just the site (`head-office`);
/// an edge carries a partition so its stream is distinguishable from the hub's. The value is
/// validated on construction and on deserialization, so a [`NodeId`] is always safe to use as a
/// label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Parses and validates a node id.
    pub fn new(value: &str) -> Result<Self> {
        let (site, partition) = match value.split_once('#') {
            Some((site, partition)) => (site, Some(partition)),
            None => (value, None),
        };
        if !valid_host_name(site) {
            return Err(Error::invalid_argument(
                "a node id's site is a lower-case host name (a-z, 0-9, '-', '.')",
            ));
        }
        if let Some(partition) = partition
            && !valid_storage_name(partition)
        {
            return Err(Error::invalid_argument(
                "a node id's partition is a storage name: [a-z][a-z0-9_]*, at most 63 bytes",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// The node id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The site part.
    pub fn site(&self) -> &str {
        self.0
            .split_once('#')
            .map_or(self.0.as_str(), |(site, _)| site)
    }

    /// The partition part, if the node carries one.
    pub fn partition(&self) -> Option<&str> {
        self.0.split_once('#').map(|(_, partition)| partition)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NodeId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for NodeId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(&value)
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

/// Whether `name` is a lower-case DNS host name: 1 to 253 bytes of dot-separated labels, each 1
/// to 63 bytes of `a-z`, `0-9` and `-`, not starting or ending with `-`.
pub(crate) fn valid_host_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// Whether `name` is a storage name: `[a-z][a-z0-9_]*`, at most 63 bytes.
pub(crate) fn valid_storage_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// One protocol version, a small monotonic integer.
///
/// Versions are negotiated by range, never by string: the wire carries a number, and both peers
/// pick the highest they share. An additive change bumps the number; a peer that does not know a
/// number simply does not offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    /// The first protocol version.
    pub const FIRST: ProtocolVersion = ProtocolVersion(1);

    /// A version from its number.
    pub const fn new(number: u16) -> Self {
        Self(number)
    }

    /// The number on the wire.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The next version.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The version this crate implements.
pub const CURRENT_PROTOCOL: ProtocolVersion = ProtocolVersion::FIRST;

/// A peer's inclusive range of supported protocol versions.
///
/// `min` is the oldest version still spoken (for backward compatibility) and `max` the newest.
/// [`negotiate`](ProtocolRange::negotiate) returns the highest version both ranges contain, which
/// is what the hub answers in a handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProtocolRange {
    /// The oldest version the peer speaks.
    pub min: ProtocolVersion,
    /// The newest version the peer speaks.
    pub max: ProtocolVersion,
}

impl ProtocolRange {
    /// A range; fails unless `min <= max`.
    pub fn new(min: ProtocolVersion, max: ProtocolVersion) -> Result<Self> {
        if min > max {
            return Err(Error::invalid_argument(
                "a protocol range's minimum is not above its maximum",
            ));
        }
        Ok(Self { min, max })
    }

    /// The range of a peer that speaks only the current protocol version.
    pub const fn current() -> Self {
        Self {
            min: CURRENT_PROTOCOL,
            max: CURRENT_PROTOCOL,
        }
    }

    /// A single-version range.
    pub const fn only(version: ProtocolVersion) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    /// The highest version both this range and `peer` contain, or `None` when they do not overlap.
    pub fn negotiate(&self, peer: &ProtocolRange) -> Option<ProtocolVersion> {
        let lowest = self.min.max(peer.min);
        let highest = self.max.min(peer.max);
        (lowest <= highest).then_some(highest)
    }

    /// Whether `version` is inside the range.
    pub fn contains(&self, version: ProtocolVersion) -> bool {
        self.min <= version && version <= self.max
    }
}

impl fmt::Display for ProtocolRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{}..={}", self.min, self.max)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn node_ids_accept_hosts_and_partitions() {
        for ok in ["head-office", "edge-khi#branch_01", "a.b.c", "n1"] {
            assert!(NodeId::new(ok).is_ok(), "{ok} should be accepted");
        }
        assert_eq!(NodeId::new("edge#p1").unwrap().partition(), Some("p1"));
    }

    #[test]
    fn node_ids_reject_anything_else() {
        for bad in [
            "", "Upper", "a..b", "-a", "a-", "a b", "x#", "x#Upper", "x#1bad",
        ] {
            assert!(NodeId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(NodeId::new("a".repeat(254).as_str()).is_err());
    }

    #[test]
    fn negotiation_picks_the_highest_shared_version() {
        let a = ProtocolRange::new(ProtocolVersion::new(1), ProtocolVersion::new(4)).unwrap();
        let b = ProtocolRange::new(ProtocolVersion::new(3), ProtocolVersion::new(9)).unwrap();
        assert_eq!(a.negotiate(&b), Some(ProtocolVersion::new(4)));
        let c = ProtocolRange::new(ProtocolVersion::new(5), ProtocolVersion::new(6)).unwrap();
        assert_eq!(a.negotiate(&c), None);
        assert!(ProtocolRange::only(ProtocolVersion::new(2)).contains(ProtocolVersion::new(2)));
    }
}
