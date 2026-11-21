//! Typed errors with stable `graph_index.*` codes (ADR-26).
//!
//! Callers match on [`GraphIndexError::code`], never on the message. Every code is catalogued
//! in `spot-errors`' `catalogue/graph_index.toml`.
//!
//! A **gap** is the one error that leaves state behind: the index cannot know what it missed,
//! so it marks the graph [`Stale`](crate::GraphIndex::is_stale) and refuses to answer from it
//! until a [`rebuild`](crate::IndexStore::rebuild) clears it. Everything else is a rejected
//! input that changes nothing.

use crate::key::GraphKey;

/// The index received an event whose sequence skips ahead of the watermark.
pub const GAP: &str = "graph_index.gap";
/// An event names a graph that is not the index's own.
pub const WRONG_GRAPH: &str = "graph_index.wrong_graph";
/// The memory budget cannot be met: every graph is pinned and the cap is already exceeded.
pub const BUDGET_EXCEEDED: &str = "graph_index.budget_exceeded";
/// A rebuild source failed, or returned edges that cannot form an index.
pub const SOURCE: &str = "graph_index.source";
/// An edge event is malformed (an empty edge-type name, or a self-edge where none is allowed).
pub const INVALID_EDGE: &str = "graph_index.invalid_edge";

/// Every error the graph index returns.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphIndexError {
    /// An event's sequence number is beyond `watermark + 1`: events were missed. The graph is
    /// marked `Stale` and answers nothing until it is rebuilt.
    #[error("graph `{graph}` missed events: expected sequence {expected}, got {got}")]
    Gap {
        /// The graph whose stream had the hole.
        graph: GraphKey,
        /// The next sequence the index could apply (`watermark + 1`).
        expected: u64,
        /// The sequence the event carried.
        got: u64,
    },

    /// An event names a graph other than the index's own.
    #[error("event names graph `{got}` but this index is `{expected}`")]
    WrongGraph {
        /// The index's graph.
        expected: GraphKey,
        /// The graph the event named.
        got: GraphKey,
    },

    /// The cap cannot be met without dropping a pinned graph.
    #[error(
        "graph index memory cap of {cap} bytes cannot be met: {used} bytes used and every graph is pinned"
    )]
    BudgetExceeded {
        /// The configured cap in bytes.
        cap: u64,
        /// The bytes the store currently holds.
        used: u64,
    },

    /// A rebuild source failed, or its edges cannot form an index.
    #[error("the graph source failed: {reason}")]
    Source {
        /// Plain-language reason; never a secret or a raw query.
        reason: String,
    },

    /// An edge event is malformed.
    #[error("the edge event is invalid: {reason}")]
    InvalidEdge {
        /// Plain-language reason.
        reason: String,
    },
}

impl GraphIndexError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Gap { .. } => GAP,
            Self::WrongGraph { .. } => WRONG_GRAPH,
            Self::BudgetExceeded { .. } => BUDGET_EXCEEDED,
            Self::Source { .. } => SOURCE,
            Self::InvalidEdge { .. } => INVALID_EDGE,
        }
    }

    /// A source failure with a plain-language reason.
    #[must_use]
    pub fn source(reason: impl Into<String>) -> Self {
        Self::Source {
            reason: reason.into(),
        }
    }

    /// A malformed edge event with a plain-language reason.
    #[must_use]
    pub fn invalid_edge(reason: impl Into<String>) -> Self {
        Self::InvalidEdge {
            reason: reason.into(),
        }
    }
}
