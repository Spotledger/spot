//! [`Cluster`]: a group of [`crate::Node`]s built from the same repository checkout.

use std::path::{Path, PathBuf};

use crate::node::{Node, NodeSpec};

/// A cluster of nodes. Dropping it drops every node (each node's own `Drop` removes its
/// database and root, or keeps them with `L3_KEEP=1`); there is nothing cluster-wide left to
/// clean up beyond that.
pub struct Cluster {
    repo: PathBuf,
    nodes: Vec<Node>,
}

impl Cluster {
    /// An empty cluster, rooted at this repository checkout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            repo: repo(),
            nodes: Vec::new(),
        }
    }

    /// Starts a node from `spec` and adds it to the cluster. `None` (nothing added) when no
    /// PostgreSQL is configured; the caller follows the same convention
    /// `spot_postgres::testing::TestDb::create` does (print the reason, skip the test, do not
    /// fake a connection).
    pub async fn add_node(&mut self, spec: NodeSpec) -> Option<&Node> {
        let node = Node::create(&self.repo, spec).await?;
        self.nodes.push(node);
        self.nodes.last()
    }

    /// The node named `name`.
    ///
    /// # Panics
    /// When no node of that name was added: a test asking for a node it never created is a
    /// bug in the test, not a runtime condition to handle.
    #[must_use]
    pub fn node(&self, name: &str) -> &Node {
        self.nodes
            .iter()
            .find(|node| node.name() == name)
            .unwrap_or_else(|| panic!("no node named `{name}` in this cluster"))
    }

    /// The node named `name`, mutably (for lifecycle calls).
    ///
    /// # Panics
    /// When no node of that name was added.
    pub fn node_mut(&mut self, name: &str) -> &mut Node {
        self.nodes
            .iter_mut()
            .find(|node| node.name() == name)
            .unwrap_or_else(|| panic!("no node named `{name}` in this cluster"))
    }

    /// Every node, in the order they were added.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}

impl Default for Cluster {
    fn default() -> Self {
        Self::new()
    }
}

/// The repository root: this crate lives at `harness/rust/l3-journal-sync`.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}
