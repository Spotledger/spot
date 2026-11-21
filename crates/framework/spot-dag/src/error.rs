//! Typed errors for `spot-dag` (brief §2 style: one enum, stable codes).
//!
//! Every variant carries the identifiers the caller needs to locate the
//! problem. Callers match on [`DagError::code`], never on the message: the
//! codes are this crate's API contract and never change in a breaking way.
//!
//! Validation collects **all** errors (a `Vec<DagError>`) instead of stopping
//! at the first; a single [`DagError`] is returned from parsing and building.

use crate::model::NodeId;

/// One problem with a DAG definition, a compile or a registry call.
///
/// `code()` returns a stable machine-readable `dag.<snake_case>` code.
/// [`DagError`] is `#[non_exhaustive]`: new variants may be added and callers
/// must keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DagError {
    /// A node id does not match `[a-z][a-z0-9_]{0,63}`.
    #[error("invalid node id `{raw}`: must match [a-z][a-z0-9_]{{0,63}}")]
    NodeIdInvalid {
        /// The rejected id text.
        raw: String,
    },
    /// A `kind` or `key` does not match `[a-z][a-z0-9_.]{0,63}`.
    #[error("invalid kind `{raw}`: must match [a-z][a-z0-9_.]{{0,63}}")]
    KindInvalid {
        /// The rejected kind/key text.
        raw: String,
    },
    /// A float was found inside a node's `config` (ADR-12: money and
    /// quantities are never floats; JSON config numbers must be integers).
    #[error("float in config of node `{node}` at `{path}`")]
    FloatInConfig {
        /// The node holding the offending config.
        node: NodeId,
        /// Path to the float inside the config, e.g. `prices[2].amount`.
        path: Box<str>,
    },
    /// The definition has zero nodes.
    #[error("definition has no nodes")]
    Empty,
    /// The definition's version is 0; versions start at 1.
    #[error("version is 0; versions start at 1")]
    VersionZero,
    /// The same node id appears twice in `nodes`.
    #[error("duplicate node `{0}`")]
    DuplicateNode(NodeId),
    /// The same `(from, to, condition)` edge appears twice in `edges`.
    #[error("duplicate edge `{from}` -> `{to}`{}",
        .condition.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())]
    DuplicateEdge {
        /// The edge endpoint the duplicate starts at.
        from: NodeId,
        /// The edge endpoint the duplicate ends at.
        to: NodeId,
        /// The duplicate's condition, when it has one.
        condition: Option<String>,
    },
    /// An edge endpoint names a node that is not in `nodes`.
    #[error("edge {edge_index} references unknown node `{id}`")]
    UnknownNode {
        /// Position of the offending edge in `edges` (0-based).
        edge_index: usize,
        /// The endpoint not present in `nodes`.
        id: NodeId,
    },
    /// An edge runs from a node to itself.
    #[error("self loop on node `{0}`")]
    SelfLoop(NodeId),
    /// The graph is cyclic; `path` is one concrete cycle, first node repeated
    /// at the end (e.g. `["a", "b", "a"]`). One error per strongly connected
    /// component.
    #[error("cycle: {}", .path.iter().map(ToString::to_string).collect::<Vec<_>>().join(" -> "))]
    Cycle {
        /// The cycle's node ids, starting and ending on the same node.
        path: Vec<NodeId>,
    },
    /// No node is free of incoming edges, so nothing can start.
    #[error("no entry node: every node has an incoming edge")]
    NoEntry,
    /// No node is free of outgoing edges, so nothing can finish.
    #[error("no terminal node: every node has an outgoing edge")]
    NoTerminal,
    /// A node cannot be reached from any entry node.
    #[error("node `{0}` is not reachable from any entry node")]
    Unreachable(NodeId),
    /// A node cannot reach any terminal node.
    #[error("node `{0}` cannot reach any terminal node")]
    DeadEnd(NodeId),
    /// More nodes than `Limits::max_nodes`.
    #[error("definition has {actual} nodes, over the limit of {limit}")]
    TooManyNodes {
        /// The configured maximum.
        limit: usize,
        /// The actual count.
        actual: usize,
    },
    /// More edges than `Limits::max_edges`.
    #[error("definition has {actual} edges, over the limit of {limit}")]
    TooManyEdges {
        /// The configured maximum.
        limit: usize,
        /// The actual count.
        actual: usize,
    },
    /// One node's serialised config is larger than
    /// `Limits::max_config_bytes`.
    #[error("config of node `{node}` is {actual} bytes, over the limit of {limit}")]
    ConfigTooLarge {
        /// The node holding the oversized config.
        node: NodeId,
        /// The configured maximum in bytes.
        limit: usize,
        /// The actual serialised size in bytes.
        actual: usize,
    },
    /// The definition text was not a valid `DagDefinition` document.
    #[error("not a valid DAG definition: {message}")]
    Json {
        /// The serde error's message (already names the offending key/path).
        message: String,
    },
}

impl DagError {
    /// The stable machine-readable code for this error, for logs, API
    /// responses and metrics. Codes are `dag.<snake_case>` and never change.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeIdInvalid { .. } => "dag.node_id_invalid",
            Self::KindInvalid { .. } => "dag.kind_invalid",
            Self::FloatInConfig { .. } => "dag.float_in_config",
            Self::Empty => "dag.empty",
            Self::VersionZero => "dag.version_zero",
            Self::DuplicateNode(_) => "dag.duplicate_node",
            Self::DuplicateEdge { .. } => "dag.duplicate_edge",
            Self::UnknownNode { .. } => "dag.unknown_node",
            Self::SelfLoop(_) => "dag.self_loop",
            Self::Cycle { .. } => "dag.cycle",
            Self::NoEntry => "dag.no_entry",
            Self::NoTerminal => "dag.no_terminal",
            Self::Unreachable(_) => "dag.unreachable",
            Self::DeadEnd(_) => "dag.dead_end",
            Self::TooManyNodes { .. } => "dag.too_many_nodes",
            Self::TooManyEdges { .. } => "dag.too_many_edges",
            Self::ConfigTooLarge { .. } => "dag.config_too_large",
            Self::Json { .. } => "dag.json",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        let cases: Vec<(DagError, &str)> = vec![
            (
                DagError::NodeIdInvalid {
                    raw: "X".to_owned(),
                },
                "dag.node_id_invalid",
            ),
            (
                DagError::KindInvalid {
                    raw: "X".to_owned(),
                },
                "dag.kind_invalid",
            ),
            (
                DagError::FloatInConfig {
                    node: NodeId::new("n").unwrap(),
                    path: "a.b".into(),
                },
                "dag.float_in_config",
            ),
            (DagError::Empty, "dag.empty"),
            (DagError::VersionZero, "dag.version_zero"),
            (
                DagError::DuplicateNode(NodeId::new("n").unwrap()),
                "dag.duplicate_node",
            ),
            (
                DagError::DuplicateEdge {
                    from: NodeId::new("a").unwrap(),
                    to: NodeId::new("b").unwrap(),
                    condition: None,
                },
                "dag.duplicate_edge",
            ),
            (
                DagError::UnknownNode {
                    edge_index: 0,
                    id: NodeId::new("x").unwrap(),
                },
                "dag.unknown_node",
            ),
            (
                DagError::SelfLoop(NodeId::new("n").unwrap()),
                "dag.self_loop",
            ),
            (
                DagError::Cycle {
                    path: vec![NodeId::new("a").unwrap(), NodeId::new("a").unwrap()],
                },
                "dag.cycle",
            ),
            (DagError::NoEntry, "dag.no_entry"),
            (DagError::NoTerminal, "dag.no_terminal"),
            (
                DagError::Unreachable(NodeId::new("n").unwrap()),
                "dag.unreachable",
            ),
            (DagError::DeadEnd(NodeId::new("n").unwrap()), "dag.dead_end"),
            (
                DagError::TooManyNodes {
                    limit: 1,
                    actual: 2,
                },
                "dag.too_many_nodes",
            ),
            (
                DagError::TooManyEdges {
                    limit: 1,
                    actual: 2,
                },
                "dag.too_many_edges",
            ),
            (
                DagError::ConfigTooLarge {
                    node: NodeId::new("n").unwrap(),
                    limit: 1,
                    actual: 2,
                },
                "dag.config_too_large",
            ),
            (
                DagError::Json {
                    message: "bad".to_owned(),
                },
                "dag.json",
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (error, code) in &cases {
            assert_eq!(&error.code(), code, "code drifted for {error:?}");
            assert!(seen.insert(*code), "duplicate code {code}");
        }
        assert_eq!(cases.len(), 18, "a variant is missing from the list");
    }

    #[test]
    fn codes_are_snake_case_after_dag_dot() {
        let variants: [DagError; 18] = [
            DagError::NodeIdInvalid {
                raw: "x".to_owned(),
            },
            DagError::KindInvalid {
                raw: "x".to_owned(),
            },
            DagError::FloatInConfig {
                node: NodeId::new("n").unwrap(),
                path: "a".into(),
            },
            DagError::Empty,
            DagError::VersionZero,
            DagError::DuplicateNode(NodeId::new("n").unwrap()),
            DagError::DuplicateEdge {
                from: NodeId::new("a").unwrap(),
                to: NodeId::new("b").unwrap(),
                condition: None,
            },
            DagError::UnknownNode {
                edge_index: 0,
                id: NodeId::new("x").unwrap(),
            },
            DagError::SelfLoop(NodeId::new("n").unwrap()),
            DagError::Cycle { path: vec![] },
            DagError::NoEntry,
            DagError::NoTerminal,
            DagError::Unreachable(NodeId::new("n").unwrap()),
            DagError::DeadEnd(NodeId::new("n").unwrap()),
            DagError::TooManyNodes {
                limit: 0,
                actual: 0,
            },
            DagError::TooManyEdges {
                limit: 0,
                actual: 0,
            },
            DagError::ConfigTooLarge {
                node: NodeId::new("n").unwrap(),
                limit: 0,
                actual: 0,
            },
            DagError::Json {
                message: String::new(),
            },
        ];
        for error in variants {
            let code = error.code();
            // `dag.` prefix, then snake_case: lowercase letters and underscores only.
            assert!(code.starts_with("dag."), "{code}");
            let tail = &code["dag.".len()..];
            assert!(!tail.is_empty(), "{code}");
            for c in tail.chars() {
                assert!(
                    c.is_ascii_lowercase() || c == '_',
                    "{code} is not snake_case"
                );
            }
        }
    }
}
