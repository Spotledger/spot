//! Typed errors for `spot-graph-algo` (one enum with stable `graph.*` codes,
//! house style of `spot-dag::error`).
//!
//! Callers match on [`GraphError::code`], never on the message: the codes are
//! this crate's API contract and never change in a breaking way.

use crate::view::NodeIx;

/// One witness cycle found by [`crate::topological_order`] or
/// [`crate::find_cycle`].
///
/// `nodes` is the cycle as a node list with the first node repeated at the
/// end: a self loop on `7` is `[7, 7]`, the two-cycle `3 -> 9 -> 3` is
/// `[3, 9, 3]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// The cycle's node ids, starting and ending on the same node.
    pub nodes: Vec<NodeIx>,
}

/// Which entry of [`crate::Limits`] was exceeded.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// `GraphView::node_count()` exceeded `Limits::max_nodes`.
    Nodes,
    /// The traversal expanded more distinct nodes than `Limits::max_visited`.
    Visited,
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nodes => f.write_str("nodes"),
            Self::Visited => f.write_str("visited"),
        }
    }
}

/// One problem with a graph or a call into this crate.
///
/// [`GraphError`] is `#[non_exhaustive]`: new variants may be added and
/// callers must keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A [`crate::Limits`] bound was exceeded.
    #[error("{kind} limit exceeded: {actual} over the limit of {limit}")]
    LimitExceeded {
        /// Which limit was hit.
        kind: LimitKind,
        /// The configured maximum.
        limit: usize,
        /// The offending count.
        actual: usize,
    },
    /// A node index is not a valid dense id for the graph.
    #[error("node index {index} is out of range for a graph with {node_count} nodes")]
    NodeOutOfRange {
        /// The rejected index.
        index: NodeIx,
        /// The graph's node count.
        node_count: usize,
    },
    /// A graph was built with more nodes than `NodeIx` (`u32`) can address.
    #[error(
        "{count} nodes cannot be addressed by the u32 `NodeIx` (limit {})",
        u32::MAX
    )]
    NodeIndexOverflow {
        /// The rejected node count.
        count: usize,
    },
    /// A path cost exceeds `u64::MAX`; costs never wrap (brief §2).
    #[error("path cost overflows u64 at edge {from} -> {to}: overflow is an error, not a wrap")]
    CostOverflow {
        /// The tail of the offending edge.
        from: NodeIx,
        /// The head of the offending edge.
        to: NodeIx,
    },
    /// An edge has weight `0` where a weight of at least 1 is required
    /// (currently: `crate::shortest_path` — its extraction contract needs
    /// strictly positive weights).
    #[error("edge {from} -> {to} has weight 0; shortest_path requires weights >= 1")]
    ZeroWeightEdge {
        /// The tail of the offending edge.
        from: NodeIx,
        /// The head of the offending edge.
        to: NodeIx,
    },
    /// A documented internal invariant did not hold. Defensive: unreachable
    /// for a well-formed [`crate::GraphView`] (one whose `successors` are
    /// stable between calls), returned instead of looping or panicking.
    #[error("internal invariant broken: {what}")]
    InternalInvariant {
        /// What went wrong, for logs only (match on this variant's `code`).
        what: &'static str,
    },
    /// The graph is cyclic; the payload is one witness cycle (brief §2).
    #[error("graph is cyclic; witness: {}",
        .0.nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" -> "))]
    Cycle(
        /// The witness cycle.
        Cycle,
    ),
}

impl GraphError {
    /// The stable machine-readable code for this error, for logs, API
    /// responses and metrics. Codes are `graph.<snake_case>` and never change.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LimitExceeded { .. } => "graph.limit_exceeded",
            Self::NodeOutOfRange { .. } => "graph.node_out_of_range",
            Self::NodeIndexOverflow { .. } => "graph.node_index_overflow",
            Self::CostOverflow { .. } => "graph.cost_overflow",
            Self::ZeroWeightEdge { .. } => "graph.zero_weight_edge",
            Self::InternalInvariant { .. } => "graph.internal_invariant",
            Self::Cycle(_) => "graph.cycle",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_snake_case() {
        let cases: Vec<(GraphError, &str)> = vec![
            (
                GraphError::LimitExceeded {
                    kind: LimitKind::Nodes,
                    limit: 1,
                    actual: 2,
                },
                "graph.limit_exceeded",
            ),
            (
                GraphError::NodeOutOfRange {
                    index: 9,
                    node_count: 3,
                },
                "graph.node_out_of_range",
            ),
            (
                GraphError::NodeIndexOverflow { count: 1 << 40 },
                "graph.node_index_overflow",
            ),
            (
                GraphError::CostOverflow { from: 0, to: 1 },
                "graph.cost_overflow",
            ),
            (
                GraphError::ZeroWeightEdge { from: 0, to: 1 },
                "graph.zero_weight_edge",
            ),
            (
                GraphError::InternalInvariant { what: "x" },
                "graph.internal_invariant",
            ),
            (
                GraphError::Cycle(Cycle { nodes: vec![3, 3] }),
                "graph.cycle",
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (error, code) in &cases {
            assert_eq!(&error.code(), code, "code drifted for {error:?}");
            assert!(seen.insert(*code), "duplicate code {code}");
            assert!(code.starts_with("graph."), "{code}");
            let tail = &code["graph.".len()..];
            for c in tail.chars() {
                assert!(c.is_ascii_lowercase() || c == '_', "{code} not snake_case");
            }
        }
        assert_eq!(cases.len(), 7, "a variant is missing from the list");
    }
}
