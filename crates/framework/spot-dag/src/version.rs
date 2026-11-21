//! Versioning (brief §4): the canonical content hash and the in-memory
//! version registry.
//!
//! The same definition always hashes the same: nodes are sorted by id, edges
//! by `(from, to, condition)` and `config` object keys are sorted, with no
//! whitespace. The definition's `version` number is **not** hashed, so
//! republishing identical content is detectable and the registry can say
//! "no change" instead of storing a copy.
//!
//! [`DagRegistry`] keeps every published version of every key in memory
//! (`&mut self` API, no locking — ADR-13: the registry is not shared state
//! here; ownership is per session).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::compile::CompiledDag;
use crate::error::DagError;
use crate::model::{DagDefinition, Node};
use crate::validate::Limits;

/// SHA-256 content hash of a definition's canonical form.
///
/// Display is 64 lowercase hex characters. Two definitions that differ only in
/// node/edge order or config key order have the same hash; any real change
/// (a node kind, an edge condition, a config value) changes it. The `version`
/// number is not part of the hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHash(String);

impl ContentHash {
    /// Hashes a definition's canonical form (see [`canonical_bytes`]).
    #[must_use]
    pub fn of(def: &DagDefinition) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(canonical_bytes(def));
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

/// Lowercase hex of a 32-byte digest, written without float or fancy code.
fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Serialises a definition into its canonical byte form:
///
/// - `key` first (the key itself, not hashed separately — every version of a
///   key hashes the same way because the registry compares hashes within one
///   key; a definition compiled under a different key with the same graph gets
///   the same hash, which the brief's "same content" wording reads as
///   intended: the hash is of the graph content);
/// - nodes sorted by id;
/// - for each node: id, kind, join (as `all`/`any`), then config serialised
///   with object keys sorted and no whitespace;
/// - edges sorted by `(from, to, condition)`;
/// - a newline after every record so concatenation is unambiguous.
fn canonical_bytes(def: &DagDefinition) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(def.key.as_bytes());
    out.push(b'\n');

    let mut nodes: Vec<&Node> = def.nodes.iter().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in nodes {
        out.extend_from_slice(b"node\t");
        out.extend_from_slice(node.id.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(node.kind.as_bytes());
        out.push(b'\t');
        out.push(match node.join {
            crate::model::JoinRule::All => b'a',
            crate::model::JoinRule::Any => b'y',
        });
        out.push(b'\t');
        out.extend_from_slice(&sorted_json(&node.config));
        out.push(b'\n');
    }

    let mut edges: Vec<&crate::model::Edge> = def.edges.iter().collect();
    edges.sort_by(|a, b| (&a.from, &a.to, &a.condition).cmp(&(&b.from, &b.to, &b.condition)));
    for edge in edges {
        out.extend_from_slice(b"edge\t");
        out.extend_from_slice(edge.from.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.to.as_str().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(edge.condition.as_deref().unwrap_or_default().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Serialises a [`serde_json::Value`] with object keys sorted and no
/// whitespace. Numbers pass through exactly as serde_json prints them (the
/// model already rejects floats).
fn sorted_json(value: &serde_json::Value) -> Vec<u8> {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut out = Vec::from(&b"{"[..]);
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                // serde_json escapes the key the same way it escapes
                // strings; reuse its writer via a throwaway object? No:
                // build it by hand with `serde_json::to_string` for the key.
                out.extend_from_slice(serde_json::to_string(key).unwrap_or_default().as_bytes());
                out.push(b':');
                out.extend_from_slice(&sorted_json(&map[key.as_str()]));
            }
            out.push(b'}');
            out
        }
        Value::Array(items) => {
            let mut out = Vec::from(&b"["[..]);
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&sorted_json(item));
            }
            out.push(b']');
            out
        }
        other => serde_json::to_vec(other).unwrap_or_default(),
    }
}

/// What a successful publish returns: the stored version and its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The version that was stored (the definition's own version number).
    pub version: u32,
    /// The content hash of the published definition.
    pub hash: ContentHash,
}

/// Errors of [`DagRegistry::publish`].
///
/// Distinct from [`DagError`]: the registry can fail structurally (a version
/// gap, a no-change republish) even when the definition itself is fine, and
/// callers match on [`RegistryError::code`], never on the message.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The definition failed validation; the whole list is returned.
    #[error("definition failed validation")]
    Invalid(Vec<DagError>),
    /// The version must be `expected` (the first version is 1, later ones are
    /// `latest + 1`) but `got` was supplied. Nothing was stored.
    #[error("version gap: expected {expected}, got {got}")]
    VersionGap {
        /// The version that had to be supplied.
        expected: u32,
        /// The version that was supplied.
        got: u32,
    },
    /// The content hash equals the previous version's; nothing was stored
    /// and the previous version stays the latest.
    #[error("no change since version {version}")]
    NoChange {
        /// The version supplied (equal in content to the previous one).
        version: u32,
    },
}

impl RegistryError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "dag.registry.invalid",
            Self::VersionGap { .. } => "dag.registry.version_gap",
            Self::NoChange { .. } => "dag.registry.no_change",
        }
    }
}

/// In-memory registry of compiled DAG versions, keyed by definition key.
///
/// `&mut self` API, no locking (brief §4). Every version that was accepted
/// stays available; the stored [`CompiledDag`]s are immutable and `Arc`-ed,
/// so a held reference keeps running while newer versions are published.
#[derive(Debug, Default)]
pub struct DagRegistry {
    by_key: BTreeMap<String, KeyVersions>,
}

/// One key's version chain.
#[derive(Debug, Default)]
struct KeyVersions {
    latest: Option<u32>,
    versions: BTreeMap<u32, Arc<CompiledDag>>,
}

impl DagRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and stores a definition as the next version of its key.
    ///
    /// Rules (brief §4):
    /// - the first version of a key must be `1`, and every later version must
    ///   be exactly `latest + 1` ([`RegistryError::VersionGap`] names both);
    /// - a definition whose content hash equals the previous version's is
    ///   rejected with [`RegistryError::NoChange`] and **not** stored;
    /// - validation errors come back as [`RegistryError::Invalid`].
    ///
    /// # Errors
    /// See above; nothing is stored on any error.
    pub fn publish(
        &mut self,
        def: &DagDefinition,
        limits: &Limits,
    ) -> Result<Published, RegistryError> {
        let compiled = CompiledDag::compile(def, limits).map_err(RegistryError::Invalid)?;
        let entry = self.by_key.entry(def.key.clone()).or_default();
        let latest = entry.latest;
        match latest {
            None => {
                if def.version != 1 {
                    return Err(RegistryError::VersionGap {
                        expected: 1,
                        got: def.version,
                    });
                }
            }
            Some(expected_minus_one) => {
                if def.version != expected_minus_one + 1 {
                    return Err(RegistryError::VersionGap {
                        expected: expected_minus_one + 1,
                        got: def.version,
                    });
                }
            }
        }
        // No-change check against the previous version's hash.
        if let Some(prev) = latest.and_then(|v| entry.versions.get(&v).map(Arc::as_ref))
            && prev.hash() == compiled.hash()
        {
            return Err(RegistryError::NoChange {
                version: def.version,
            });
        }
        let hash = compiled.hash().clone();
        let version = def.version;
        entry.latest = Some(version);
        entry.versions.insert(version, Arc::new(compiled));
        Ok(Published { version, hash })
    }

    /// A stored version of a key.
    #[must_use]
    pub fn get(&self, key: &str, version: u32) -> Option<Arc<CompiledDag>> {
        self.by_key.get(key)?.versions.get(&version).cloned()
    }

    /// The latest compiled version of a key, when any was published.
    #[must_use]
    pub fn latest(&self, key: &str) -> Option<Arc<CompiledDag>> {
        let entry = self.by_key.get(key)?;
        entry.latest.and_then(|v| entry.versions.get(&v).cloned())
    }

    /// Every stored version of a key, ascending.
    #[must_use]
    pub fn versions(&self, key: &str) -> Vec<u32> {
        self.by_key
            .get(key)
            .map(|e| e.versions.keys().copied().collect())
            .unwrap_or_default()
    }
}
