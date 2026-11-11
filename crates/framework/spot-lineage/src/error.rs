//! Typed errors of the lineage layer, with stable `lineage.*` codes.
//!
//! A graph query's own failures ([`GraphQueryError`]) pass through unchanged, so a
//! `storage.traversal_too_large` stays itself and is never turned into an empty answer. Callers
//! match on [`LineageError::code`], never on the text.

use spot_graph_query::GraphQueryError;

/// Every error the lineage layer returns.
#[derive(Debug, thiserror::Error)]
pub enum LineageError {
    /// The chain configuration text is invalid.
    #[error("the lineage chain configuration is invalid at `{path}`: {message}")]
    Config {
        /// Where in the configuration text: a dotted path such as `chain[0].edges`.
        path: String,
        /// What is wrong, in plain words.
        message: String,
    },

    /// No document chain is configured for the DocType.
    #[error("no document chain is configured for DocType `{doctype}`")]
    UnknownChain {
        /// The DocType the query started from.
        doctype: String,
    },

    /// No bill of materials is configured for the item DocType.
    #[error("no bill of materials is configured for item DocType `{doctype}`")]
    UnknownBom {
        /// The item DocType the query started from.
        doctype: String,
    },

    /// A requested depth is outside the graph query API's range.
    #[error("max_depth must be 1 to {max}, not {depth}")]
    DepthOutOfRange {
        /// The depth the query asked for.
        depth: u16,
        /// The graph query API's maximum depth.
        max: u16,
    },

    /// A requested node budget is outside the graph query API's range.
    #[error("max_nodes must be 1 to {max}, not {max_nodes}")]
    NodeBudgetOutOfRange {
        /// The budget the caller asked for.
        max_nodes: u32,
        /// The graph query API's largest node budget.
        max: u32,
    },

    /// A stored or configured value is not a plain decimal quantity.
    #[error("`{text}` is not a plain decimal quantity")]
    BadQuantity {
        /// The offending text.
        text: String,
    },

    /// A node on a bill of materials carries no decimal quantity.
    #[error("the field `{field}` carries no decimal quantity on a bill-of-materials node")]
    MissingQuantity {
        /// The field the configuration names for the per-unit quantity.
        field: String,
    },

    /// An exact quantity multiplication or addition did not fit.
    #[error("an exact quantity operation did not fit; reduce the scale or the magnitude")]
    QuantityOverflow,

    /// A graph query failed; the code is the graph query API's own.
    #[error(transparent)]
    Graph(#[from] GraphQueryError),
}

impl LineageError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Config { .. } => "lineage.config",
            Self::UnknownChain { .. } => "lineage.unknown_chain",
            Self::UnknownBom { .. } => "lineage.unknown_bom",
            Self::DepthOutOfRange { .. } => "lineage.depth_out_of_range",
            Self::NodeBudgetOutOfRange { .. } => "lineage.max_nodes_out_of_range",
            Self::BadQuantity { .. } => "lineage.bad_quantity",
            Self::MissingQuantity { .. } => "lineage.missing_quantity",
            Self::QuantityOverflow => "lineage.quantity_overflow",
            Self::Graph(error) => error.code(),
        }
    }
}
