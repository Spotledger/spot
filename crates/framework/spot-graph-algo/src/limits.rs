//! Caller-supplied work bounds ([`Limits`]) checked by every algorithm in
//! this crate, so a caller can bound work on untrusted graphs.

use crate::error::{GraphError, LimitKind};

/// Bounds on the work an algorithm may do. Every public function takes a
/// `&Limits` and returns [`GraphError::LimitExceeded`] when a bound is hit.
///
/// - `max_nodes` bounds `GraphView::node_count()`: a graph over the limit is
///   rejected before any traversal starts.
/// - `max_visited` bounds how many distinct nodes the traversal *expands*
///   (scans the outgoing edges of). Reading the graph's edge set in one
///   `O(V + E)` pass (building in-degrees, detecting unit weights) is not
///   counted; `max_nodes` bounds that pass.
///
/// [`Limits::default`] is unlimited, so plain callers never see a limit
/// error; the 1M-node scale tests run under the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum `GraphView::node_count()` accepted, `usize::MAX` = unlimited.
    pub max_nodes: usize,
    /// Maximum number of distinct nodes expanded during a traversal,
    /// `usize::MAX` = unlimited.
    pub max_visited: usize,
}

impl Limits {
    /// No bounds at all: every graph and every traversal size is accepted.
    pub const UNLIMITED: Limits = Limits {
        max_nodes: usize::MAX,
        max_visited: usize::MAX,
    };

    /// Both bounds set explicitly.
    #[must_use]
    pub const fn new(max_nodes: usize, max_visited: usize) -> Limits {
        Limits {
            max_nodes,
            max_visited,
        }
    }

    /// Unlimited nodes, bounded traversal expansions.
    #[must_use]
    pub const fn bounded_visited(max_visited: usize) -> Limits {
        Limits {
            max_nodes: usize::MAX,
            max_visited,
        }
    }
}

impl Default for Limits {
    /// Unlimited — identical to [`Limits::UNLIMITED`].
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// Rejects a graph whose node count exceeds `limits.max_nodes`, returning
/// the (validated) count for convenience.
pub(crate) fn ensure_node_count(node_count: usize, limits: &Limits) -> Result<usize, GraphError> {
    if node_count > limits.max_nodes {
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Nodes,
            limit: limits.max_nodes,
            actual: node_count,
        })
    } else {
        Ok(node_count)
    }
}

/// Counts one expanded node against `limits.max_visited`.
///
/// `expanded` is the running count for the current call (including this
/// node); returns the matching [`GraphError::LimitExceeded`] when the bound
/// is crossed.
pub(crate) fn ensure_visited(expanded: usize, limits: &Limits) -> Result<(), GraphError> {
    if expanded > limits.max_visited {
        Err(GraphError::LimitExceeded {
            kind: LimitKind::Visited,
            limit: limits.max_visited,
            actual: expanded,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unlimited_and_checks_pass() {
        let limits = Limits::default();
        assert_eq!(limits, Limits::UNLIMITED);
        assert!(ensure_node_count(usize::MAX, &limits).is_ok());
        assert!(ensure_visited(usize::MAX, &limits).is_ok());
    }

    #[test]
    fn node_limit_reports_the_offending_count() {
        let limits = Limits::new(10, usize::MAX);
        assert!(ensure_node_count(10, &limits).is_ok());
        assert_eq!(
            ensure_node_count(11, &limits),
            Err(GraphError::LimitExceeded {
                kind: LimitKind::Nodes,
                limit: 10,
                actual: 11,
            })
        );
    }

    #[test]
    fn visited_limit_counts_the_crossing_node() {
        let limits = Limits::bounded_visited(3);
        assert!(ensure_visited(3, &limits).is_ok());
        assert_eq!(
            ensure_visited(4, &limits),
            Err(GraphError::LimitExceeded {
                kind: LimitKind::Visited,
                limit: 3,
                actual: 4,
            })
        );
    }
}
