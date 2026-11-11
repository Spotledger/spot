//! The graph's identity: one named graph inside one tenant.
//!
//! A tenant holds several named graphs (for example `hierarchies` and `boms`); the index, the
//! budget and every event address a graph by this pair, never by a bare name.

use serde::{Deserialize, Serialize};

/// The identity of one graph: `(tenant, graph name)`.
///
/// Both parts are opaque, non-empty strings. A tenant is the tenancy the graph belongs to
/// (ADR-70); the graph name is the collection the caller asked for. Neither is a secret.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GraphKey {
    /// The tenant the graph belongs to.
    pub tenant: String,
    /// The graph's name within the tenant.
    pub graph: String,
}

impl GraphKey {
    /// A key for `tenant`'s `graph`.
    #[must_use]
    pub fn new(tenant: impl Into<String>, graph: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            graph: graph.into(),
        }
    }
}

impl std::fmt::Display for GraphKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.tenant, self.graph)
    }
}
