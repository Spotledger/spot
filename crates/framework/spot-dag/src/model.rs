//! The typed definition model (brief §1): [`DagDefinition`] and its parts.
//!
//! Every type derives `serde` with `deny_unknown_fields`, so a definition that
//! carries a key this crate does not know is rejected at parse time, not
//! silently ignored. Grammar checks (`NodeId`, `kind`, `key`) run when a value
//! is built in code and when it is deserialised, so JSON input and hand-built
//! values satisfy the same contract.
//!
//! `config` is arbitrary `serde_json::Value` data, but JSON numbers in it must
//! be integers-or-strings: a float anywhere inside `config` is rejected with
//! [`crate::DagError::FloatInConfig`] (ADR-12: money and quantities are never
//! floats). The crate itself never contains a float literal.

use serde::{Deserialize, Serialize};

use crate::error::DagError;

/// Maximum length of a [`NodeId`], a `kind` or a `key` (64 chars).
const MAX_NAME_LEN: usize = 64;

/// The identifier of a DAG node, with its grammar baked in.
///
/// Valid ids match `[a-z][a-z0-9_]{0,63}`: a lowercase letter first, then
/// lowercase letters, digits or underscores, at most 64 characters. Build one
/// with [`NodeId::new`]; deserialising validates too, so a bad id in JSON is
/// an error before it ever becomes a node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// Validates the grammar and builds the id.
    ///
    /// # Errors
    /// Returns [`DagError::NodeIdInvalid`] when `raw` does not match
    /// `[a-z][a-z0-9_]{0,63}`.
    pub fn new(raw: &str) -> Result<Self, DagError> {
        if is_valid_id(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(DagError::NodeIdInvalid {
                raw: raw.to_owned(),
            })
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for NodeId {
    type Error = DagError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl core::fmt::Display for NodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

/// One grammar for all name-shaped strings: lowercase first letter, then
/// lowercase letters, digits, underscores (and dots when `allow_dots`), at
/// most 64 characters. `allow_dots` is false for [`NodeId`], true for `kind`
/// and `key`.
fn is_valid_name(raw: &str, allow_dots: bool) -> bool {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || raw.len() > MAX_NAME_LEN {
        return false;
    }
    chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || (allow_dots && c == '.')
    })
}

/// Whether `raw` matches the [`NodeId`] grammar `[a-z][a-z0-9_]{0,63}`.
fn is_valid_id(raw: &str) -> bool {
    is_valid_name(raw, false)
}

/// Whether `raw` matches the `kind`/`key` grammar `[a-z][a-z0-9_.]{0,63}`.
fn is_valid_kind(raw: &str) -> bool {
    is_valid_name(raw, true)
}

/// Builds a [`NodeId`] that is already known to satisfy the grammar.
///
/// # Panics
/// Only from misuse inside this crate; every caller passes ids that passed
/// `is_valid_id` first. Debug builds assert the contract.
#[must_use]
fn node_id_from_valid(raw: String) -> NodeId {
    debug_assert!(is_valid_id(&raw), "id `{raw}` must already be valid");
    NodeId(raw)
}

/// Validates an id-shaped string where deserialisation needs a [`DagError`].
fn validate_node_id(raw: &str) -> Result<(), DagError> {
    if is_valid_id(raw) {
        Ok(())
    } else {
        Err(DagError::NodeIdInvalid {
            raw: raw.to_owned(),
        })
    }
}

/// Validates a kind-shaped string where deserialisation needs a [`DagError`].
fn validate_kind(raw: &str) -> Result<(), DagError> {
    if is_valid_kind(raw) {
        Ok(())
    } else {
        Err(DagError::KindInvalid {
            raw: raw.to_owned(),
        })
    }
}

/// Walks a `config` [`serde_json::Value`] looking for a float: any JSON number
/// that is not an integer (`1` passes, `1.5` fails). Returns the path of the
/// first offending number, `None` when there is none. Nested arrays and
/// objects produce `prices[2].amount`-style paths. Also used by `validate` so
/// programmatically built values are checked uniformly.
pub(crate) fn find_float(value: &serde_json::Value, path: String) -> Option<String> {
    match value {
        serde_json::Value::Number(n) if n.as_i64().is_none() => Some(path),
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| find_float(item, format!("{path}[{i}]"))),
        serde_json::Value::Object(map) => map.iter().find_map(|(k, item)| {
            find_float(
                item,
                if path.is_empty() {
                    (*k).clone()
                } else {
                    format!("{path}.{k}")
                },
            )
        }),
        _ => None,
    }
}

/// Validates `config` for one node: no float anywhere inside it.
///
/// # Errors
/// [`DagError::FloatInConfig`] naming the node and the path inside its config.
fn validate_config(node: &NodeId, config: &serde_json::Value) -> Result<(), DagError> {
    match find_float(config, String::new()) {
        Some(path) => Err(DagError::FloatInConfig {
            node: node.clone(),
            path: Box::<str>::from(path.as_str()),
        }),
        None => Ok(()),
    }
}

/// One node of the graph.
///
/// `kind` is opaque to this crate (the engine decides what `"approval"`
/// means); it must match `[a-z][a-z0-9_.]{0,63}`. `config` carries node data
/// as JSON: floats anywhere in it are rejected. `join` decides how many
/// incoming edges must be taken before the node may start; `All` (the
/// default) wants every edge, `Any` one is enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// The node's identifier ([`NodeId`] grammar).
    pub id: NodeId,
    /// What the node does; opaque here, `[a-z][a-z0-9_.]{0,63}`.
    pub kind: String,
    /// How many incoming edges must be taken before the node may start.
    #[serde(default)]
    pub join: JoinRule,
    /// Node data; JSON integers, strings, bools, null, arrays and objects
    /// only — no floats.
    #[serde(default, deserialize_with = "deny_floats")]
    pub config: serde_json::Value,
}

impl Node {
    /// Builds a node, validating `id`, `kind` and `config`.
    ///
    /// # Errors
    /// Returns [`DagError::NodeIdInvalid`], [`DagError::KindInvalid`] or
    /// [`DagError::FloatInConfig`] for the first problem found.
    pub fn new(
        id: NodeId,
        kind: impl Into<String>,
        join: JoinRule,
        config: serde_json::Value,
    ) -> Result<Self, DagError> {
        let kind = kind.into();
        validate_kind(&kind)?;
        validate_config(&id, &config)?;
        Ok(Self {
            id,
            kind,
            join,
            config,
        })
    }
}

/// The manual [`Deserialize`] for [`Node`]: keep `deny_unknown_fields` and the
/// field defaults from a derived mirror, then run the grammar check on `id`.
impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawNode {
            id: String,
            kind: String,
            #[serde(default)]
            join: JoinRule,
            #[serde(default, deserialize_with = "self::deny_floats")]
            config: serde_json::Value,
        }

        let raw = RawNode::deserialize(deserializer)?;
        validate_node_id(&raw.id).map_err(serde::de::Error::custom)?;
        validate_kind(&raw.kind).map_err(serde::de::Error::custom)?;
        Ok(Node {
            id: node_id_from_valid(raw.id),
            kind: raw.kind,
            join: raw.join,
            config: raw.config,
        })
    }
}

/// Deserialises a `config` value rejecting float numbers (ADR-12).
fn deny_floats<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match find_float(&value, String::new()) {
        Some(path) => Err(serde::de::Error::custom(format!(
            "float in config at `{path}`"
        ))),
        None => Ok(value),
    }
}

/// How many incoming edges must be taken before a node may start.
///
/// `All` (the default) waits for every incoming edge; `Any` starts on the
/// first taken one. On a node with fewer than two incoming edges both mean
/// the same thing and both are allowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum JoinRule {
    /// Every incoming edge must be taken before the node may start.
    #[default]
    All,
    /// One taken incoming edge is enough.
    Any,
}

/// One directed edge of the graph.
///
/// `condition` is an opaque label such as `"approved"`; this crate never
/// evaluates it. Two edges with the same `(from, to, condition)` are a
/// duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// Where the edge starts.
    pub from: NodeId,
    /// Where the edge ends.
    pub to: NodeId,
    /// Opaque label; absent for an unconditional edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

impl Edge {
    /// Builds an edge; the ids must already be valid [`NodeId`]s.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId, condition: Option<String>) -> Self {
        Self {
            from,
            to,
            condition,
        }
    }
}

/// The manual [`Deserialize`] for [`Edge`]: keep `deny_unknown_fields`, then
/// run the grammar check on both endpoints.
impl<'de> Deserialize<'de> for Edge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawEdge {
            from: String,
            to: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            condition: Option<String>,
        }

        let raw = RawEdge::deserialize(deserializer)?;
        validate_node_id(&raw.from).map_err(serde::de::Error::custom)?;
        validate_node_id(&raw.to).map_err(serde::de::Error::custom)?;
        Ok(Edge {
            from: node_id_from_valid(raw.from),
            to: node_id_from_valid(raw.to),
            condition: raw.condition,
        })
    }
}

/// A whole DAG definition: a key (the DAG's name within its app), a version
/// number that starts at 1, and the nodes and edges that make up the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagDefinition {
    /// The DAG's name; `[a-z][a-z0-9_.]{0,63}` (dots allowed, like `kind`).
    pub key: String,
    /// The version number; starts at 1, never 0 (checked by `validate`).
    pub version: u32,
    /// The nodes, in definition order.
    pub nodes: Vec<Node>,
    /// The edges, in definition order.
    pub edges: Vec<Edge>,
}

impl DagDefinition {
    /// Builds a definition from parts without judging the graph shape (that
    /// is [`crate::validate`]'s job). `key` grammar, `version >= 1`, floats
    /// and node ids are the caller's contract here.
    #[must_use]
    pub fn new(key: impl Into<String>, version: u32, nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        Self {
            key: key.into(),
            version,
            nodes,
            edges,
        }
    }

    /// Builds a definition in code, validating every node's config and the
    /// key grammar.
    ///
    /// # Errors
    /// Returns [`DagError::FloatInConfig`] for the first node whose config
    /// carries a float, or [`DagError::KindInvalid`] when the key breaks the
    /// grammar.
    pub fn try_new(
        key: impl Into<String>,
        version: u32,
        nodes: Vec<Node>,
        edges: Vec<Edge>,
    ) -> Result<Self, DagError> {
        let key = key.into();
        validate_kind(&key)?;
        for node in &nodes {
            validate_config(&node.id, &node.config)?;
        }
        Ok(Self {
            key,
            version,
            nodes,
            edges,
        })
    }

    /// Parses a definition from JSON text.
    ///
    /// Unknown fields are rejected (`deny_unknown_fields`), node ids and
    /// kinds are grammar-checked, and floats in configs are rejected — the
    /// same contract as deserialising straight through serde, in one call.
    ///
    /// # Errors
    /// [`DagError::Json`] when the text is not a definition document, or the
    /// model errors (bad id/kind, float in config) when content is invalid.
    ///
    /// # Panics
    /// Never.
    pub fn from_json(text: &str) -> Result<Self, DagError> {
        serde_json::from_str(text).map_err(|e| DagError::Json {
            message: e.to_string(),
        })
    }

    /// Serialises the definition to JSON. Structural order (nodes, edges,
    /// config keys) is the definition's own; content equivalence across
    /// orderings is [`crate::version::ContentHash`]'s job.
    ///
    /// # Errors
    /// JSON serialisation of this model cannot fail; the signature keeps
    /// parity with [`DagDefinition::from_json`].
    ///
    /// # Panics
    /// Never.
    pub fn to_json(&self) -> Result<String, DagError> {
        serde_json::to_string(self).map_err(|e| DagError::Json {
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_node_ids() {
        for good in [
            "a",
            "start_here",
            "n0",
            "x63charsofverylongidentifierwithunderscorezzz",
        ] {
            assert!(NodeId::new(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn rejects_bad_node_ids() {
        for bad in [
            "",
            "A",
            "1abc",
            "-abc",
            "a b",
            "a.B",
            "ä",
            "aB",
            // 65 lowercase characters: one too many.
            "a0123456789012345678901234567890123456789012345678901234567890123",
        ] {
            assert!(NodeId::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn kind_allows_dots() {
        let node = Node::new(
            NodeId::new("n1").unwrap(),
            "approval.step",
            JoinRule::Any,
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(node.kind, "approval.step");
        assert!(
            Node::new(
                NodeId::new("n1").unwrap(),
                "Approval",
                JoinRule::All,
                serde_json::Value::Null
            )
            .is_err()
        );
    }
}
