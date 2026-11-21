//! The stable text dump ([`GraphModel::to_text`](crate::GraphModel::to_text)).
//!
//! Deterministic, line-based, self-quoting and greppable: nodes sorted by
//! name, edges sorted by canonical name, every value one token with
//! `"quoted"` escaping for spaces, tabs and backslashes. A golden test pins
//! the bytes; the version line says which dump format wrote them.
//!
//! The dump is **not** a parse format — [`serde`](serde) is the round-trip
//! contract; this is the human-checkable projection of it.

use std::fmt::Write as _;

use crate::model::{GraphModel, NodeType, TargetShape};

/// The dump format's version, printed on the first line.
pub const GRAPH_TEXT_VERSION: &str = "spot-graph-text-1";

impl GraphModel {
    /// The stable text dump: version header, meta version, node types, edge
    /// types — every section sorted by name.
    ///
    /// The `meta_version` line records the metadata the model was derived
    /// from; it is informational, so two dumps that differ only in it are not
    /// a content change ([`GraphModel::diff`] ignores it).
    #[must_use]
    pub fn to_text(&self) -> String {
        // Headers, a name per line, edges with properties: generous slack.
        let mut out = String::with_capacity(4096);
        let _ = writeln!(out, "{GRAPH_TEXT_VERSION}");
        let _ = writeln!(out, "meta_version {}", self.meta_version);
        for node in self.nodes.values() {
            write_node(&mut out, node);
        }
        for edge in self.edges.values() {
            write_edge(&mut out, edge);
        }
        out
    }
}

/// One `"quoted"` token of the dump, as `to_text` writes every value.
fn token(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            ' ' => out.push_str("\\s"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_node(out: &mut String, node: &NodeType) {
    let _ = writeln!(
        out,
        "node {} label {} posted {}",
        token(&node.name),
        token(&node.label),
        node.is_posted
    );
}

fn write_edge(out: &mut String, edge: &crate::model::EdgeType) {
    // The stable short line: every tool of the pipeline greps this shape.
    let _ = writeln!(
        out,
        "edge {} {} {} -> {} required {}",
        token(&edge.name),
        edge.cardinality.as_str(),
        edge.ownership.as_str(),
        token(&edge.from),
        edge.required
    );
    let _ = writeln!(out, "  source {}", edge.source.as_str());
    match &edge.to {
        TargetShape::One(to) => {
            let _ = writeln!(out, "  to one {}", token(to));
        }
        TargetShape::AnyOf(targets) => {
            let _ = writeln!(out, "  to any_of {}", targets.len());
            for target in targets {
                let _ = writeln!(out, "    {}", token(target));
            }
        }
        TargetShape::Dynamic => {
            let _ = writeln!(out, "  to dynamic");
        }
    }
    if let Some(relation) = &edge.relation {
        let _ = writeln!(
            out,
            "  relation {} from {} to {}",
            token(&relation.doctype),
            token(&relation.from_field),
            token(&relation.to_field)
        );
        if let Some(aggregate) = &relation.aggregate {
            let _ = writeln!(
                out,
                "  aggregate {} {}",
                aggregate.how.as_str(),
                token(&aggregate.property)
            );
        }
    }
    if !edge.properties.is_empty() {
        let _ = writeln!(out, "  properties {}", edge.properties.len());
        for property in &edge.properties {
            let _ = writeln!(
                out,
                "    {} kind {} required {} options {}",
                token(&property.name),
                property.kind.as_str(),
                property.required,
                match &property.options {
                    Some(options) => token(options),
                    None => "-".to_owned(),
                }
            );
        }
    }
}
