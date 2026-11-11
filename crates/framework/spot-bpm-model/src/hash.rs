//! The content hash of a process version (brief §4): identical content hashes
//! identically, whatever the definition order, version number or status.
//!
//! The hash covers the graph *content*: the process name and app, every node
//! with its kind, join rule, terminal flag, error branch, timeout and retry
//! policy, every edge and every rule. It deliberately excludes the version
//! number, the status and the title, so republishing identical content is
//! detectable and a draft and a published version of the same text hash the
//! same.

use core::fmt;

use sha2::{Digest, Sha256};

use crate::model::{Edge, JoinRule, Node, ProcessVersion, Rule};

/// SHA-256 content hash of a process version's canonical form.
///
/// Display is 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    /// Hashes a version's canonical content (see [`canonical_bytes`]).
    #[must_use]
    pub fn of(version: &ProcessVersion) -> Self {
        Self::of_canonical(&canonical_bytes(version))
    }

    /// Hashes a canonical byte form the caller builds.
    ///
    /// A process contract (`spot-bpm-contract`, WP-BPM-12) hashes its own
    /// canonical form this way, so a contract and a process version are hashed
    /// by one type and one algorithm.
    #[must_use]
    pub fn of_canonical(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex(&hasher.finalize()))
    }

    /// The 64 lowercase hex characters.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase hex of a 32-byte digest.
fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// The canonical byte form of a version's content: a fixed preamble, then
/// nodes sorted by id, edges sorted by `(from, to, condition, rule, label,
/// default)` and rules sorted by id, one tab-separated record per line.
fn canonical_bytes(version: &ProcessVersion) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"bpm-model-v1\n");
    out.extend_from_slice(b"name\t");
    out.extend_from_slice(version.name.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(b"app\t");
    out.extend_from_slice(version.app.as_bytes());
    out.push(b'\n');

    let mut nodes: Vec<&Node> = version.nodes.iter().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in nodes {
        out.extend_from_slice(b"node\t");
        out.extend_from_slice(node.id.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(node.kind.name().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(join_token(node.join).as_bytes());
        out.push(b'\t');
        out.push(if node.terminal { b't' } else { b'f' });
        out.push(b'\t');
        out.extend_from_slice(node.error.as_ref().map_or("", |e| e.as_str()).as_bytes());
        out.push(b'\t');
        write_u64(&mut out, node.timeout_secs);
        out.push(b'\t');
        out.extend_from_slice(node.retry.max_attempts.to_string().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(node.retry.delay_secs.to_string().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(
            crate::tomlutil::render_inline_table(&node.kind.config_table()).as_bytes(),
        );
        out.push(b'\n');
    }

    let mut edges: Vec<&Edge> = version.edges.iter().collect();
    edges.sort_by(|a, b| {
        (&a.from, &a.to, &a.condition, &a.rule, &a.label, a.default).cmp(&(
            &b.from,
            &b.to,
            &b.condition,
            &b.rule,
            &b.label,
            b.default,
        ))
    });
    for edge in edges {
        out.extend_from_slice(b"edge\t");
        out.extend_from_slice(edge.from.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.to.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.condition.as_deref().unwrap_or_default().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.rule.as_deref().unwrap_or_default().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.label.as_deref().unwrap_or_default().as_bytes());
        out.push(b'\t');
        out.push(if edge.default { b't' } else { b'f' });
        out.push(b'\n');
    }

    let mut rules: Vec<&Rule> = version.rules.iter().collect();
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    for rule in rules {
        out.extend_from_slice(b"rule\t");
        out.extend_from_slice(rule.id.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(rule.expression.as_bytes());
        out.push(b'\n');
    }
    out
}

/// The join rule as one token.
fn join_token(join: JoinRule) -> String {
    match join {
        JoinRule::All => "all".to_owned(),
        JoinRule::Any => "any".to_owned(),
        JoinRule::Quorum { required } => format!("q:{required}"),
    }
}

/// Writes an optional integer or `-`.
fn write_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => out.extend_from_slice(v.to_string().as_bytes()),
        None => out.push(b'-'),
    }
}
