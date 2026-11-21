//! The compiler (brief §4): validate, then build a `spot_dag::DagDefinition`
//! and a side table of per-node config, and compile the definition onto the DAG
//! runtime.
//!
//! This crate never re-implements the DAG layer (ADR-13): a process is mapped
//! to `spot-dag`'s typed nodes and edges and handed to its compiler. Because a
//! process graph is compiled onto an acyclic runtime, validation rejects cycles
//! first, and compilation is the second, cheap check.
//!
//! Everything here is pure and synchronous: no clock, no I/O, no randomness.
//! The content hash is identical for identical text (see
//! [`ProcessVersion::content_hash`](crate::ProcessVersion::content_hash)).

use std::collections::BTreeMap;

use crate::hash::ContentHash;
use crate::model::{AllowAllMeta, Edge, JoinRule, Limits, MetaView, NodeKind, ProcessVersion};
use crate::validate::{Problem, codes, validate_version};

/// The opaque condition the DAG layer carries for an error branch.
const ERROR_CONDITION: &str = "error";

/// A compiled process: the DAG definition, its compiled form and the side table
/// of per-node config the engine reads (the DAG layer keeps node data opaque).
#[derive(Debug, Clone)]
pub struct CompiledProcess {
    /// The definition handed to the DAG runtime.
    pub definition: spot_dag::DagDefinition,
    /// The compiled DAG with the indexes a runtime needs.
    pub dag: spot_dag::CompiledDag,
    /// Per-node config, by node id.
    pub nodes: BTreeMap<crate::id::NodeId, NodeKind>,
    /// The version number compiled.
    pub version: u32,
    /// The content hash of the compiled version.
    pub hash: ContentHash,
}

/// Compiles a version onto the DAG runtime, validating first against a
/// permissive metadata view (compile is about structure; a host that has the
/// DocType metadata calls [`compile_checked`] to also check DocTypes).
///
/// # Errors
/// Every [`Problem`] validation found, or `bpm_model.dag_invalid` if the DAG
/// layer rejects the graph (a backstop — validation runs first).
pub fn compile(version: &ProcessVersion) -> Result<CompiledProcess, Vec<Problem>> {
    compile_checked(version, &AllowAllMeta, &Limits::new())
}

/// Compiles a version, validating against the given metadata view and limits.
///
/// # Errors
/// Every [`Problem`] validation found, or `bpm_model.dag_invalid`.
pub fn compile_checked(
    version: &ProcessVersion,
    meta: &dyn MetaView,
    limits: &Limits,
) -> Result<CompiledProcess, Vec<Problem>> {
    let problems = validate_version(version, meta, limits);
    if !problems.is_empty() {
        return Err(problems);
    }

    let mut nodes = Vec::with_capacity(version.nodes.len());
    let mut side_table = BTreeMap::new();
    for node in &version.nodes {
        let id = spot_dag::NodeId::new(node.id.as_str()).map_err(|_| dag_problem(version))?;
        let dag_node = spot_dag::Node::new(
            id,
            node.kind.name(),
            map_join(node.join),
            node_config_json(node),
        )
        .map_err(|_| dag_problem(version))?;
        nodes.push(dag_node);
        side_table.insert(node.id.clone(), node.kind.clone());
    }

    let mut edges = Vec::with_capacity(version.edges.len() + version.nodes.len());
    for edge in &version.edges {
        let from = spot_dag::NodeId::new(edge.from.as_str()).map_err(|_| dag_problem(version))?;
        let to = spot_dag::NodeId::new(edge.to.as_str()).map_err(|_| dag_problem(version))?;
        edges.push(spot_dag::Edge::new(from, to, edge_condition(edge)));
    }
    // An error branch is a real outgoing edge of its node: the DAG runtime has
    // no separate error concept, and validation counted it the same way.
    for node in &version.nodes {
        if let Some(target) = &node.error {
            let from = spot_dag::NodeId::new(node.id.as_str()).map_err(|_| dag_problem(version))?;
            let to = spot_dag::NodeId::new(target.as_str()).map_err(|_| dag_problem(version))?;
            edges.push(spot_dag::Edge::new(
                from,
                to,
                Some(ERROR_CONDITION.to_owned()),
            ));
        }
    }

    let definition =
        spot_dag::DagDefinition::try_new(version.name.clone(), version.version, nodes, edges)
            .map_err(|_| dag_problem(version))?;

    let dag = spot_dag::CompiledDag::compile(&definition, &spot_dag::Limits::new()).map_err(
        |errors| {
            errors
                .into_iter()
                .map(|_| dag_problem(version))
                .collect::<Vec<_>>()
        },
    )?;

    Ok(CompiledProcess {
        definition,
        dag,
        nodes: side_table,
        version: version.version,
        hash: version.content_hash(),
    })
}

/// The opaque condition `spot-dag` carries for an edge: the condition
/// expression when there is one, otherwise the rule name, otherwise none.
fn edge_condition(edge: &Edge) -> Option<String> {
    edge.condition.clone().or_else(|| edge.rule.clone())
}

/// Maps the process join rule onto the DAG layer's `all`/`any`. A quorum of one
/// is `any`; every stricter quorum (and `all`) is `all`, which never starts a
/// node before its edges are taken. The exact quorum stays in the side table.
fn map_join(join: JoinRule) -> spot_dag::JoinRule {
    match join {
        JoinRule::All => spot_dag::JoinRule::All,
        JoinRule::Any => spot_dag::JoinRule::Any,
        JoinRule::Quorum { required } if required <= 1 => spot_dag::JoinRule::Any,
        JoinRule::Quorum { .. } => spot_dag::JoinRule::All,
    }
}

/// A node's config as the JSON the DAG layer carries, with no floats (the
/// process model never has one).
fn node_config_json(node: &crate::model::Node) -> serde_json::Value {
    serde_json::Value::Object(
        node.kind
            .config_table()
            .iter()
            .map(|(k, v)| (k.clone(), toml_to_json(v)))
            .collect(),
    )
}

/// Converts a TOML value to JSON (a datetime becomes its text).
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// A backstop problem when the DAG layer rejects a graph validation accepted.
fn dag_problem(version: &ProcessVersion) -> Problem {
    Problem::new(codes::DAG_INVALID, version.version, None, None)
}
