//! Typed errors and derive problems for `spot-graph-model`: stable `graph.*` codes.
//!
//! `derive` itself never fails — bad declarations come back as [`Problem`]s
//! from [`GraphModel::problems`](crate::GraphModel::problems), so a model over
//! unfinished metadata is still buildable and diffable. [`GraphModelError`] is
//! for the boundaries that *must* fail: duplicate edge names where a caller
//! would otherwise silently lose an edge.
//!
//! Callers match on the code, never on the message: the codes are this
//! crate's API contract.

/// One problem of a derive. Never a panic: the model is built, the problem is
/// reported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum Problem {
    /// A Link or Table field's `options` name a DocType that does not exist,
    /// or a Link names a child DocType (spot-meta would have refused these;
    /// an in-memory `EffectiveMeta` built by hand can still carry them).
    #[error("`{doctype}.{field}` links to unknown DocType `{target}`")]
    DanglingTarget {
        /// The DocType carrying the field.
        doctype: String,
        /// The field.
        field: String,
        /// The target `options` named.
        target: String,
    },

    /// A relation spec names a DocType that is not in the metadata; the spec
    /// was ignored.
    #[error("relation spec names unknown DocType `{doctype}`")]
    UnknownRelationDoctype {
        /// The DocType the spec named.
        doctype: String,
    },

    /// A relation spec names a DocType that is a child; children cannot carry
    /// the two ends of a relation. The spec was ignored.
    #[error("relation spec names child DocType `{doctype}`")]
    RelationDoctypeIsChild {
        /// The DocType the spec named.
        doctype: String,
    },

    /// The relation DocType's `from_field` or `to_field` is missing or not a
    /// Link field. The spec was ignored and the DocType was left a node.
    #[error("relation spec field `{field}` of `{doctype}` is missing or not a link")]
    RelationFieldNotLink {
        /// The relation DocType.
        doctype: String,
        /// The field (`from_field` or `to_field`).
        field: String,
    },

    /// A non-endpoint field of a relation DocType has a type the graph query cannot return as
    /// an edge property. The field was left out of the edge's properties.
    #[error(
        "relation DocType `{doctype}` field `{field}` has type `{field_type}`, which an edge cannot carry"
    )]
    RelationPropertyUnsupported {
        /// The relation DocType.
        doctype: String,
        /// The field.
        field: String,
        /// The field's type, as its definition text spells it.
        field_type: String,
    },

    /// The relation spec's target DocType does not exist; the edge was built
    /// with `Dynamic` target and the problem reported.
    #[error("relation spec end `{field}` of `{doctype}` links to unknown DocType `{target}`")]
    RelationEndUnknownTarget {
        /// The relation DocType.
        doctype: String,
        /// The end field.
        field: String,
        /// The target the end field named.
        target: String,
    },

    /// A relation spec's `aggregate` names a property the relation DocType does not declare. The
    /// edge was built without the aggregate, so repeated lines stay ambiguous.
    #[error("aggregate property `{field}` is not a field of relation DocType `{doctype}`")]
    AggregatePropertyUnknown {
        /// The relation DocType.
        doctype: String,
        /// The property the spec named.
        field: String,
    },

    /// A relation spec's `aggregate` names a field that is not an `Int` or a `Decimal`, or one
    /// of the two link ends. The edge was built without the aggregate.
    #[error(
        "aggregate property `{field}` of relation DocType `{doctype}` is not an int or a decimal"
    )]
    AggregatePropertyNotNumeric {
        /// The relation DocType.
        doctype: String,
        /// The property the spec named.
        field: String,
    },

    /// The spec's `edge_name` duplicates another edge's canonical name;
    /// neither can be told apart in lookups, so the later spec was refused.
    #[error("edge name `{0}` is used twice")]
    DuplicateEdgeName(String),
}

impl Problem {
    /// The stable machine-readable code for this problem.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DanglingTarget { .. } => "graph.dangling_target",
            Self::UnknownRelationDoctype { .. } => "graph.relation_doctype_unknown",
            Self::RelationDoctypeIsChild { .. } => "graph.relation_doctype_is_child",
            Self::RelationFieldNotLink { .. } => "graph.relation_field_not_link",
            Self::RelationPropertyUnsupported { .. } => "graph.relation_property_unsupported",
            Self::RelationEndUnknownTarget { .. } => "graph.relation_end_unknown_target",
            Self::AggregatePropertyUnknown { .. } => "graph.aggregate_property_unknown",
            Self::AggregatePropertyNotNumeric { .. } => "graph.aggregate_property_not_numeric",
            Self::DuplicateEdgeName(_) => "graph.duplicate_edge_name",
        }
    }
}

/// The crate's one fatal error (duplicate canonical names in a hand-built
/// registry, where a silent merge would lose an edge).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphModelError {
    /// Two edge types with the same canonical name were registered.
    #[error("edge name `{0}` is used twice")]
    DuplicateEdgeName(String),
}

impl GraphModelError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateEdgeName(_) => "graph.duplicate_edge_name",
        }
    }
}
