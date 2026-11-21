//! The in-memory system graph snapshot and the [`SourceAdapter`] contract.
//!
//! A [`SelfModel`] is immutable between events: [`SelfModel::build`] folds every source into
//! one deterministic snapshot with an epoch. [`SelfModel::apply_event`] moves the snapshot
//! forward one event at a time for WP-AI-02.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_graph_model::{EdgeTypeId, NodeTypeId};

use crate::SelfModelError;
use crate::event::SystemEvent;
use crate::evidence::{EvidenceRef, HistoryEntry, TimeRange};
use crate::kind::{EdgeKind, NodeKind, SourceKind};

/// A node's stable id, reusing `spot-graph-model`'s SHA-256 node ids.
pub type NodeId = NodeTypeId;

/// An edge's stable id, reusing `spot-graph-model`'s SHA-256 edge ids.
pub type EdgeId = EdgeTypeId;

/// The canonical key of a node: its kind and name.
#[must_use]
pub fn node_key(kind: NodeKind, name: &str) -> String {
    format!("{}:{name}", kind.as_str())
}

/// The stable id of a node with this kind and name.
#[must_use]
pub fn node_id(kind: NodeKind, name: &str) -> NodeId {
    NodeId::of(&node_key(kind, name))
}

/// A node named by kind and name, before an id is looked up.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeRef {
    /// What the node is.
    pub kind: NodeKind,
    /// Its name in that kind's namespace.
    pub name: String,
}

impl NodeRef {
    /// A node reference.
    #[must_use]
    pub fn new(kind: NodeKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }

    /// Its canonical key.
    #[must_use]
    pub fn key(&self) -> String {
        node_key(self.kind, &self.name)
    }

    /// Its stable id.
    #[must_use]
    pub fn id(&self) -> NodeId {
        node_id(self.kind, &self.name)
    }
}

/// One node of the system graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The stable id.
    pub id: NodeId,
    /// The canonical `kind:name` key.
    pub key: String,
    /// What the node is.
    pub kind: NodeKind,
    /// Its name in that kind's namespace.
    pub name: String,
    /// A human label, when the source has one.
    pub label: Option<String>,
    /// The app that owns or declares it, when known.
    pub app: Option<String>,
    /// Non-sensitive attributes: a type, a version, a hook point, a status.
    pub attributes: BTreeMap<String, String>,
    /// The minimum agent tier that may see it.
    pub min_tier: Tier,
    /// The records that prove the node exists.
    pub evidence: Vec<EvidenceRef>,
}

impl Node {
    /// A node from an authoritative source.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::EvidenceMissing`] when `evidence` is empty or names no record.
    pub fn new(
        kind: NodeKind,
        name: impl Into<String>,
        evidence: Vec<EvidenceRef>,
    ) -> Result<Self, SelfModelError> {
        let name = name.into();
        let key = node_key(kind, &name);
        if evidence.is_empty() || evidence.iter().any(|item| !item.is_complete()) {
            return Err(SelfModelError::EvidenceMissing { what: key });
        }
        Ok(Self {
            id: NodeId::of(&key),
            key,
            kind,
            name,
            label: None,
            app: None,
            attributes: BTreeMap::new(),
            min_tier: kind.min_tier(),
            evidence,
        })
    }

    /// Sets the human label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Sets the owning app.
    #[must_use]
    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    /// Adds one non-sensitive attribute.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Raises the visibility floor above the kind's default.
    ///
    /// A source never lowers a kind's floor: a customisation is still configuration even
    /// when its underlying field is metadata.
    #[must_use]
    pub fn with_min_tier(mut self, tier: Tier) -> Self {
        self.min_tier = self.min_tier.max(tier);
        self
    }

    /// One attribute's value, when the source recorded it.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// The canonical reference to this node.
    #[must_use]
    pub fn reference(&self) -> NodeRef {
        NodeRef::new(self.kind, self.name.clone())
    }
}

/// One directed edge of the system graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The stable id.
    pub id: EdgeId,
    /// The subject.
    pub from: NodeId,
    /// The object.
    pub to: NodeId,
    /// What the subject is to the object.
    pub kind: EdgeKind,
    /// When the relation was recorded, in unix microseconds.
    pub at: Option<i64>,
    /// The records that prove the relation.
    pub evidence: Vec<EvidenceRef>,
}

impl Edge {
    /// An edge from `from` to `to` with evidence.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::EvidenceMissing`] when `evidence` is empty or names no record.
    pub fn new(
        from: NodeId,
        to: NodeId,
        kind: EdgeKind,
        at: Option<i64>,
        evidence: Vec<EvidenceRef>,
    ) -> Result<Self, SelfModelError> {
        let key = edge_key(&from, kind, &to);
        if evidence.is_empty() || evidence.iter().any(|item| !item.is_complete()) {
            return Err(SelfModelError::EvidenceMissing { what: key });
        }
        Ok(Self {
            id: EdgeId::of(&key),
            from,
            to,
            kind,
            at,
            evidence,
        })
    }

    /// The edge's canonical key.
    #[must_use]
    pub fn key(&self) -> String {
        edge_key(&self.from, self.kind, &self.to)
    }
}

fn edge_key(from: &NodeId, kind: EdgeKind, to: &NodeId) -> String {
    format!("{}|{}|{}", from.as_str(), kind.as_str(), to.as_str())
}

/// What one source adapter contributes to a snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceData {
    /// The nodes the source knows.
    pub nodes: Vec<Node>,
    /// The edges the source knows.
    pub edges: Vec<Edge>,
    /// The history entries the source knows.
    pub history: Vec<HistoryEntry>,
}

impl SourceData {
    /// Empty data.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// One authoritative source of the self-model.
///
/// Implementations are pure reads: they must not read the clock, the network or storage. The
/// `epoch` is the source's own version (a metadata version, an installer generation, a
/// telemetry sequence); the caller passes time into every record. An epoch may repeat for
/// different content (a rollback, or a state adopted from another node), so it says *when*, never
/// *what*: what a source holds is identified by the evidence ids it emits, not by its epoch.
pub trait SourceAdapter {
    /// Which source this is.
    fn source(&self) -> SourceKind;

    /// The source's own version.
    fn epoch(&self) -> u64;

    /// Reads the source's part of the graph.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::SourceInvalid`] when the source's data cannot be represented, and
    /// [`SelfModelError::EvidenceMissing`] when a record has no evidence.
    fn snapshot(&self) -> Result<SourceData, SelfModelError>;
}

/// The immutable self-model snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfModel {
    epoch: u64,
    source_versions: BTreeMap<SourceKind, u64>,
    nodes: BTreeMap<NodeId, Node>,
    edges: BTreeMap<EdgeId, Edge>,
    history: BTreeMap<NodeId, Vec<HistoryEntry>>,
}

impl SelfModel {
    /// An empty snapshot at this epoch.
    #[must_use]
    pub fn empty(epoch: u64) -> Self {
        Self {
            epoch,
            source_versions: BTreeMap::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            history: BTreeMap::new(),
        }
    }

    /// Folds every source into one snapshot.
    ///
    /// Node and edge merges are deterministic: the same sources in any order produce equal
    /// snapshots. A duplicate node merges its evidence and accepts only attributes both
    /// sides agree on; a duplicate edge merges its evidence.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::SourceInvalid`] when a source cannot be read, two sources give the
    /// same node a different attribute value, or an edge names a node no source produced.
    pub fn build(epoch: u64, sources: &[&dyn SourceAdapter]) -> Result<Self, SelfModelError> {
        let mut model = Self::empty(epoch);
        for source in sources {
            let kind = source.source();
            let source_epoch = source.epoch();
            let data = source.snapshot()?;
            model
                .source_versions
                .entry(kind)
                .and_modify(|seen| *seen = (*seen).max(source_epoch))
                .or_insert(source_epoch);
            model.merge(data, kind)?;
        }
        Ok(model)
    }

    fn merge(&mut self, data: SourceData, source: SourceKind) -> Result<(), SelfModelError> {
        for node in data.nodes {
            match self.nodes.get_mut(&node.id) {
                Some(existing) => {
                    if existing.kind != node.kind || existing.name != node.name {
                        return Err(SelfModelError::SourceInvalid {
                            source: source.as_str().to_owned(),
                            reason: format!("node id collides with {}", existing.key),
                        });
                    }
                    for (key, value) in node.attributes {
                        match existing.attributes.get(&key) {
                            Some(seen) if seen != &value => {
                                return Err(SelfModelError::SourceInvalid {
                                    source: source.as_str().to_owned(),
                                    reason: format!(
                                        "{} sets attribute `{key}` to {value:?}, not {seen:?}",
                                        existing.key
                                    ),
                                });
                            }
                            Some(_) => {}
                            None => {
                                existing.attributes.insert(key, value);
                            }
                        }
                    }
                    // The minimum is order-independent, so two sources in either order agree.
                    existing.label = merge_optional(&existing.label, &node.label);
                    existing.app = merge_optional(&existing.app, &node.app);
                    existing.min_tier = existing.min_tier.max(node.min_tier);
                    existing.evidence.extend(node.evidence);
                    normalize_evidence(&mut existing.evidence);
                }
                None => {
                    self.nodes.insert(node.id.clone(), node);
                }
            }
        }
        for edge in data.edges {
            if !self.nodes.contains_key(&edge.from) || !self.nodes.contains_key(&edge.to) {
                return Err(SelfModelError::SourceInvalid {
                    source: source.as_str().to_owned(),
                    reason: format!("edge {} has a missing endpoint", edge.key()),
                });
            }
            self.insert_edge(edge);
        }
        for entry in data.history {
            self.push_history(entry);
        }
        Ok(())
    }

    /// The snapshot's epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The version each source contributed.
    #[must_use]
    pub fn source_versions(&self) -> &BTreeMap<SourceKind, u64> {
        &self.source_versions
    }

    /// The number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// The node with this id, when it exists.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The node with this canonical reference, when it exists.
    #[must_use]
    pub fn node_ref(&self, node: &NodeRef) -> Option<&Node> {
        self.nodes.get(&node.id())
    }

    /// The node with this canonical reference.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::NodeNotFound`] when the snapshot has no such node.
    pub fn resolve(&self, node: &NodeRef) -> Result<&Node, SelfModelError> {
        self.node_ref(node)
            .ok_or_else(|| SelfModelError::NodeNotFound { node: node.key() })
    }

    /// Every node, in id order.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Every edge, in id order.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    /// Every node's history, in node id order.
    #[must_use]
    pub fn history(&self) -> &BTreeMap<NodeId, Vec<HistoryEntry>> {
        &self.history
    }

    /// One node's history, in time then record order.
    #[must_use]
    pub fn history_of(&self, node: &NodeId) -> &[HistoryEntry] {
        self.history.get(node).map_or(&[], Vec::as_slice)
    }

    /// One node's history inside a time window.
    #[must_use]
    pub fn history_between(&self, node: &NodeId, range: &TimeRange) -> Vec<&HistoryEntry> {
        self.history_of(node)
            .iter()
            .filter(|entry| range.contains(entry.at))
            .collect()
    }

    /// The edges that touch `node`.
    pub fn touching(&self, node: &NodeId) -> impl Iterator<Item = &Edge> {
        self.edges
            .values()
            .filter(move |edge| &edge.from == node || &edge.to == node)
    }

    pub(crate) fn next_epoch(&mut self) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.epoch
    }

    pub(crate) fn ensure_node(&mut self, node: Node) {
        match self.nodes.get_mut(&node.id) {
            Some(existing) => {
                for (key, value) in node.attributes {
                    existing.attributes.entry(key).or_insert(value);
                }
                existing.label = merge_optional(&existing.label, &node.label);
                existing.app = merge_optional(&existing.app, &node.app);
                existing.evidence.extend(node.evidence);
                normalize_evidence(&mut existing.evidence);
            }
            None => {
                self.nodes.insert(node.id.clone(), node);
            }
        }
    }

    pub(crate) fn insert_edge(&mut self, edge: Edge) {
        match self.edges.get_mut(&edge.id) {
            Some(existing) => {
                existing.evidence.extend(edge.evidence);
                normalize_evidence(&mut existing.evidence);
                existing.at = match (existing.at, edge.at) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (None, right) => right,
                    (left, None) => left,
                };
            }
            None => {
                self.edges.insert(edge.id.clone(), edge);
            }
        }
    }

    pub(crate) fn push_history(&mut self, entry: HistoryEntry) {
        let entries = self.history.entry(entry.node.clone()).or_default();
        entries.push(entry);
        entries.sort();
        entries.dedup();
    }

    pub(crate) fn set_attribute(
        &mut self,
        id: &NodeId,
        key: impl Into<String>,
        value: impl Into<String>,
    ) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.attributes.insert(key.into(), value.into());
        }
    }

    /// Applies one incremental system event and returns the new epoch.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::EventInvalid`] when the event carries no evidence, and
    /// [`SelfModelError::EvidenceMissing`] when an edge it would add has none.
    pub fn apply_event(&mut self, event: SystemEvent) -> Result<u64, SelfModelError> {
        crate::event::apply(self, event)
    }
}

/// The lexicographically smaller of two optional labels, so merge order never matters.
fn merge_optional(left: &Option<String>, right: &Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.clone().min(right.clone())),
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    }
}

fn normalize_evidence(evidence: &mut Vec<EvidenceRef>) {
    evidence.sort();
    evidence.dedup();
}
