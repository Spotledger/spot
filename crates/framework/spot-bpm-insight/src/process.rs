//! The process-version and step keys that identify every aggregate.

use serde::{Deserialize, Serialize};
use spot_dag::NodeId;

/// One process version: a process name and its version number.
///
/// A Case pins exactly one version, and every aggregate is reported against it, so a change to
/// the process never mixes the old and the new numbers (WP-BPM-09: a running Case stays on its
/// version).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessKey {
    /// The process name.
    pub name: String,
    /// The version number (starts at 1).
    pub version: u32,
}

impl ProcessKey {
    /// Builds a key from a process name and version.
    #[must_use]
    pub fn new(name: impl Into<String>, version: u32) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }
}

impl core::fmt::Display for ProcessKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

/// One step of one process version: the key a [`StepStats`](crate::StepStats) is reported under.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepKey {
    /// The process version the step belongs to.
    pub process: ProcessKey,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
}

impl StepKey {
    /// Builds a key from a process version and a node.
    #[must_use]
    pub fn new(process: ProcessKey, node: NodeId) -> Self {
        Self { process, node }
    }
}

impl core::fmt::Display for StepKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} step `{}`", self.process, self.node)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn keys_render_for_explanations() {
        let key = StepKey::new(
            ProcessKey::new("order_flow", 3),
            NodeId::new("approve").unwrap(),
        );
        assert_eq!(key.to_string(), "order_flow v3 step `approve`");
    }
}
