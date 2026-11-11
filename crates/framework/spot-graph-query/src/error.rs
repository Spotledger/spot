//! Typed errors of the graph query API, with stable `graphq.*` codes.
//!
//! Storage failures pass through unchanged as [`GraphQueryError::Storage`], so a
//! `TraversalTooLarge` is reported as itself and never turned into an empty answer. Callers match
//! on the code, never on the text.

use spot_storage_contract::StorageError;

/// One problem found by the builder. Every problem of a query is reported together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryProblem {
    /// The stable code, for example `graphq.no_start`.
    pub code: &'static str,
    /// A plain sentence naming the problem.
    pub message: String,
}

/// Every error the graph query API returns.
#[derive(Debug, thiserror::Error)]
pub enum GraphQueryError {
    /// The builder rejected the query. Each problem carries its own code.
    #[error("the graph query is invalid: {}", join(.0))]
    Invalid(Vec<QueryProblem>),

    /// The graph model has no edge type of that name. `similar` lists names that look alike.
    #[error("the graph model has no edge type `{name}`{}", suggest(.similar))]
    UnknownEdgeType {
        /// The name the query asked for.
        name: String,
        /// Canonical names of edge types that look similar, sorted.
        similar: Vec<String>,
    },

    /// The edge type exists but the query API cannot follow it yet.
    #[error("edge type `{name}` cannot be followed yet: {reason}")]
    UnsupportedEdge {
        /// The canonical edge-type name.
        name: String,
        /// Why it cannot be followed.
        reason: String,
    },

    /// A query names an edge property the edge does not have. At compile time when the edge type
    /// declares its properties; at run time when it does not and an edge lacks the property.
    #[error("edge type `{edge}` has no property `{property}`")]
    UnknownEdgeProperty {
        /// The canonical edge-type name.
        edge: String,
        /// The property the query named.
        property: String,
    },

    /// An edge property holds a value the query API does not support (only null, bool, integer,
    /// decimal, text, date and link values are).
    #[error(
        "property `{property}` of edge type `{edge}` holds a value type the query API does not support"
    )]
    UnsupportedEdgeValue {
        /// The canonical edge-type name.
        edge: String,
        /// The property.
        property: String,
    },

    /// The rolled-up product along a path does not fit: an integer overflow, a decimal overflow,
    /// or a result that would need more than 28 fractional digits (which would lose precision).
    #[error("the rolled-up `{property}` along `{edge}` overflows")]
    RollupOverflow {
        /// The canonical edge-type name of the hop that overflowed.
        edge: String,
        /// The rolled-up property.
        property: String,
    },

    /// The rolled-up property of an edge is not an integer or a decimal (including null).
    #[error("the rolled-up property `{property}` of edge type `{edge}` is not a number")]
    RollupNotNumeric {
        /// The canonical edge-type name.
        edge: String,
        /// The rolled-up property.
        property: String,
    },

    /// A summed rollup would evaluate more paths than `max_paths` allows. No partial sum is
    /// returned.
    #[error("the summed rollup would evaluate more than {max_paths} paths")]
    TooManyPaths {
        /// The bound the query ran under.
        max_paths: u32,
    },

    /// Two or more records of a relation DocType join the same two records along one relation edge
    /// type. Which one's properties are the edge's is not decidable, so no answer is given.
    #[error(
        "relation edge type `{edge}` joins the same two records through {count} relation records; its properties are ambiguous"
    )]
    AmbiguousRelationEdge {
        /// The canonical edge-type name.
        edge: String,
        /// How many relation records join the pair.
        count: usize,
        /// For an edge type that merges repeated lines (`aggregate`): the property whose value
        /// differs between the lines. `None` when the edge type does not merge lines.
        property: Option<String>,
    },

    /// Summing the lines of one pair overflows the integer or decimal range (or would need more
    /// precision than a decimal holds). No partial sum is returned.
    #[error("summing `{property}` over the lines of relation edge type `{edge}` overflows")]
    EdgeValueOverflow {
        /// The canonical edge-type name.
        edge: String,
        /// The aggregated property.
        property: String,
    },

    /// A storage failure, passed through unchanged.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl GraphQueryError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Invalid(problems) => problems.first().map_or("graphq.invalid", |p| p.code),
            Self::UnknownEdgeType { .. } => "graphq.unknown_edge_type",
            Self::UnsupportedEdge { .. } => "graphq.unsupported_edge",
            Self::UnknownEdgeProperty { .. } => "graphq.unknown_edge_property",
            Self::UnsupportedEdgeValue { .. } => "graphq.unsupported_edge_value",
            Self::RollupOverflow { .. } => "graphq.rollup_overflow",
            Self::RollupNotNumeric { .. } => "graphq.rollup_not_numeric",
            Self::TooManyPaths { .. } => "graphq.too_many_paths",
            Self::AmbiguousRelationEdge { .. } => "graphq.ambiguous_relation_edge",
            Self::EdgeValueOverflow { .. } => "graphq.edge_value_overflow",
            Self::Storage(error) => error.kind.code(),
        }
    }
}

fn join(problems: &[QueryProblem]) -> String {
    problems
        .iter()
        .map(|p| p.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn suggest(similar: &[String]) -> String {
    if similar.is_empty() {
        String::new()
    } else {
        format!(" (did you mean {}?)", similar.join(", "))
    }
}

/// Builds a [`QueryProblem`].
pub(crate) fn problem(code: &'static str, message: impl Into<String>) -> QueryProblem {
    QueryProblem {
        code,
        message: message.into(),
    }
}
