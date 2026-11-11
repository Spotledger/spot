//! The self-model's typed errors, each with a stable `self_model.*` code.
//!
//! The codes are catalogued in `crates/framework/spot-errors/catalogue/self_model.toml`.

use std::fmt;

use serde::Serialize;

/// The stable error codes this crate raises.
///
/// Every code has an entry in the error catalogue.
pub mod codes {
    /// A node key does not exist in this snapshot.
    pub const NODE_NOT_FOUND: &str = "self_model.node_not_found";
    /// A node kind name is not part of the self-model vocabulary.
    pub const UNKNOWN_NODE_KIND: &str = "self_model.unknown_node_kind";
    /// An edge kind name is not part of the self-model vocabulary.
    pub const UNKNOWN_EDGE_KIND: &str = "self_model.unknown_edge_kind";
    /// A node or edge has no evidence record, so it cannot explain anything.
    pub const EVIDENCE_MISSING: &str = "self_model.evidence_missing";
    /// No evidence-backed cause reaches the queried symptom.
    pub const NO_CAUSE: &str = "self_model.no_cause";
    /// A source adapter produced data the model cannot accept.
    pub const SOURCE_INVALID: &str = "self_model.source_invalid";
    /// A traversal hit the configured work bound.
    pub const LIMIT_EXCEEDED: &str = "self_model.limit_exceeded";
    /// The caller may not see the queried node.
    pub const QUERY_DENIED: &str = "self_model.query_denied";
    /// A system event cannot be applied to this snapshot.
    pub const EVENT_INVALID: &str = "self_model.event_invalid";
}

/// Everything that can go wrong while building, reading or updating a self-model.
///
/// Serialisable, so a host can emit a refusal on an event stream: the caller-visible answer to
/// a query is exactly this value, and the crate keeps the tier truth in the [`QueryRecord`]
/// instead (see [`crate::service`]).
///
/// [`QueryRecord`]: crate::QueryRecord
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SelfModelError {
    /// The node key is absent from this snapshot.
    NodeNotFound {
        /// The canonical key that was asked for.
        node: String,
    },
    /// A node kind was written in text but is not one this crate knows.
    UnknownNodeKind {
        /// The text that was offered.
        kind: String,
    },
    /// An edge kind was written in text but is not one this crate knows.
    UnknownEdgeKind {
        /// The text that was offered.
        kind: String,
    },
    /// An item that must explain something has no evidence record.
    EvidenceMissing {
        /// What lacks evidence (a node key or an edge key).
        what: String,
    },
    /// No evidence-backed cause reaches the queried symptom.
    NoCause {
        /// The symptom that has no cause.
        subject: String,
    },
    /// A source adapter produced data the model cannot accept.
    SourceInvalid {
        /// The source's stable name.
        source: String,
        /// Why its data was refused.
        reason: String,
    },
    /// A traversal hit the configured work bound.
    LimitExceeded {
        /// The bound that was hit (`nodes` or `visited`).
        kind: String,
        /// The configured maximum.
        limit: usize,
        /// The value that crossed it.
        actual: usize,
    },
    /// An internal guard: the service raised it where a query reached a node it must never
    /// answer.
    ///
    /// The introspection queries never report a tier refusal with this code: a node behind the
    /// caller's tier floor is answered exactly like one the snapshot does not hold
    /// ([`SelfModelError::NodeNotFound`]), so the answer says nothing about whether the item
    /// exists or which tier it needs (see the [`crate::service`] module docs). The tier truth is
    /// kept on the query audit sink only.
    QueryDenied {
        /// The item the caller asked for.
        node: String,
    },
    /// A system event cannot be applied to this snapshot.
    EventInvalid {
        /// Why the event was refused.
        reason: String,
    },
}

impl SelfModelError {
    /// The stable code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeNotFound { .. } => codes::NODE_NOT_FOUND,
            Self::UnknownNodeKind { .. } => codes::UNKNOWN_NODE_KIND,
            Self::UnknownEdgeKind { .. } => codes::UNKNOWN_EDGE_KIND,
            Self::EvidenceMissing { .. } => codes::EVIDENCE_MISSING,
            Self::NoCause { .. } => codes::NO_CAUSE,
            Self::SourceInvalid { .. } => codes::SOURCE_INVALID,
            Self::LimitExceeded { .. } => codes::LIMIT_EXCEEDED,
            Self::QueryDenied { .. } => codes::QUERY_DENIED,
            Self::EventInvalid { .. } => codes::EVENT_INVALID,
        }
    }
}

impl fmt::Display for SelfModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeNotFound { node } => write!(f, "the self-model has no node {node}"),
            Self::UnknownNodeKind { kind } => write!(f, "`{kind}` is not a node kind"),
            Self::UnknownEdgeKind { kind } => write!(f, "`{kind}` is not an edge kind"),
            Self::EvidenceMissing { what } => {
                write!(
                    f,
                    "{what} has no evidence record and cannot explain a change"
                )
            }
            Self::NoCause { subject } => {
                write!(f, "no evidence-backed cause reaches {subject}")
            }
            Self::SourceInvalid { source, reason } => {
                write!(f, "source {source} produced invalid data: {reason}")
            }
            Self::LimitExceeded {
                kind,
                limit,
                actual,
            } => write!(f, "the {kind} limit is {limit}, got {actual}"),
            Self::QueryDenied { node } => {
                write!(f, "the self-model could not answer a query about {node}")
            }
            Self::EventInvalid { reason } => write!(f, "the event is invalid: {reason}"),
        }
    }
}

impl std::error::Error for SelfModelError {}

impl From<spot_graph_algo::GraphError> for SelfModelError {
    fn from(error: spot_graph_algo::GraphError) -> Self {
        use spot_graph_algo::{GraphError, LimitKind};
        match error {
            GraphError::LimitExceeded {
                kind,
                limit,
                actual,
            } => Self::LimitExceeded {
                kind: match kind {
                    LimitKind::Nodes => "nodes".to_owned(),
                    LimitKind::Visited => "visited".to_owned(),
                    _ => "unknown".to_owned(),
                },
                limit,
                actual,
            },
            other => Self::SourceInvalid {
                source: "traversal".to_owned(),
                reason: other.to_string(),
            },
        }
    }
}
