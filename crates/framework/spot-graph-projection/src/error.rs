//! Typed errors with stable `graph_projection.*` codes (ADR-26).
//!
//! Callers match on [`ProjectionError::code`], never on the message. Every code is catalogued in
//! `spot-errors`' `catalogue/graph_projection.toml`.
//!
//! A **gap** and a **resync** are the two errors that leave the projection behind its source: a
//! gap means the sink missed events and must be rebuilt, a resync means the outbox stream
//! restarted at a new generation, so the durable checkpoint can no longer be trusted. Everything
//! else is a rejected input or a failed dependency.

use spot_graph_index::{GraphIndexError, GraphKey};

/// The graph sink failed (an external engine or the reference sink refused the batch).
pub const SINK: &str = "graph_projection.sink";
/// The sink missed events: the applied sequence skips ahead of its watermark.
pub const GAP: &str = "graph_projection.gap";
/// An edge event names a graph that is not the sink's own.
pub const WRONG_GRAPH: &str = "graph_projection.wrong_graph";
/// An outbox payload could not be decoded into an edge event.
pub const DECODE: &str = "graph_projection.decode";
/// The durable checkpoint store failed.
pub const CHECKPOINT: &str = "graph_projection.checkpoint";
/// The rebuild source failed, or returned edges that cannot form a graph.
pub const SOURCE: &str = "graph_projection.source";
/// The outbox stream restarted at a new generation: the projection must be rebuilt.
pub const RESYNC: &str = "graph_projection.resync";

/// Every error the graph projection returns.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionError {
    /// The sink refused the batch. Nothing was applied for that batch.
    #[error("the graph sink failed: {reason}")]
    Sink {
        /// Plain-language reason; never a secret.
        reason: String,
    },

    /// The sink received a sequence beyond its watermark plus one: events were missed. The sink
    /// must be rebuilt from its source before it is trusted again.
    #[error("graph `{graph}` missed events: expected sequence {expected}, got {got}")]
    Gap {
        /// The graph whose stream had the hole.
        graph: GraphKey,
        /// The next sequence the sink could apply (`watermark + 1`).
        expected: u64,
        /// The sequence the event carried.
        got: u64,
    },

    /// An edge event names a graph other than the sink's own.
    #[error("an edge event names graph `{got}` but the sink is `{expected}`")]
    WrongGraph {
        /// The sink's graph.
        expected: GraphKey,
        /// The graph the event named.
        got: GraphKey,
    },

    /// An outbox payload is not a valid edge event (a bad topic, schema version or shape).
    #[error("the edge event could not be decoded: {reason}")]
    Decode {
        /// Plain-language reason; never a secret.
        reason: String,
    },

    /// The durable checkpoint could not be read or written.
    #[error("the projection checkpoint failed: {reason}")]
    Checkpoint {
        /// Plain-language reason.
        reason: String,
    },

    /// The rebuild source failed, or its edges cannot form a graph.
    #[error("the projection source failed: {reason}")]
    Source {
        /// Plain-language reason.
        reason: String,
    },

    /// The outbox feed restarted at a new generation; the stored position is from another stream
    /// generation and cannot be resumed. Rebuild the projection.
    #[error("the outbox stream restarted at generation {generation}: rebuild the projection")]
    Resync {
        /// The generation the offending record carried.
        generation: u64,
    },
}

impl ProjectionError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Sink { .. } => SINK,
            Self::Gap { .. } => GAP,
            Self::WrongGraph { .. } => WRONG_GRAPH,
            Self::Decode { .. } => DECODE,
            Self::Checkpoint { .. } => CHECKPOINT,
            Self::Source { .. } => SOURCE,
            Self::Resync { .. } => RESYNC,
        }
    }

    /// A decode failure with a plain-language reason.
    #[must_use]
    pub fn decode(reason: impl Into<String>) -> Self {
        Self::Decode {
            reason: reason.into(),
        }
    }

    /// A checkpoint failure with a plain-language reason.
    #[must_use]
    pub fn checkpoint(reason: impl Into<String>) -> Self {
        Self::Checkpoint {
            reason: reason.into(),
        }
    }

    /// A sink failure with a plain-language reason.
    #[must_use]
    pub fn sink(reason: impl Into<String>) -> Self {
        Self::Sink {
            reason: reason.into(),
        }
    }

    /// A source failure with a plain-language reason.
    #[must_use]
    pub fn source(reason: impl Into<String>) -> Self {
        Self::Source {
            reason: reason.into(),
        }
    }
}

impl From<GraphIndexError> for ProjectionError {
    fn from(error: GraphIndexError) -> Self {
        match error {
            GraphIndexError::Gap {
                graph,
                expected,
                got,
            } => Self::Gap {
                graph,
                expected,
                got,
            },
            GraphIndexError::WrongGraph { expected, got } => Self::WrongGraph { expected, got },
            GraphIndexError::Source { reason } => Self::Source { reason },
            GraphIndexError::InvalidEdge { reason } => Self::Sink { reason },
            GraphIndexError::BudgetExceeded { .. } => Self::Sink {
                reason: "the sink's memory budget cannot be met".to_owned(),
            },
        }
    }
}
