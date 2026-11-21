//! The read-only introspection API: `describe`, `neighbors`, `history` and `why`.
//!
//! Every query is filtered by the caller's agent tier and recorded through a [`QueryAudit`]
//! sink. No method reads the wall clock: `at` is always an argument.
//!
//! # One answer for a hidden node and an absent one
//!
//! [`SelfModelService::visible`](SelfModelService) is the only place a query decides what a
//! caller may see. A caller below a node's tier floor is answered exactly as it would be for a
//! key that is not in the snapshot at all: the same [`SelfModelError::NodeNotFound`] value, the
//! same text and the same work path, before any traversal, history read or search starts.
//! Nothing in the answer says whether the item exists, and there is no extra path for
//! the hidden case.
//!
//! The truth is not lost, it moves to the owner: the [`QueryRecord`] the [`QueryAudit`] sink
//! receives still says [`QueryOutcome::Denied`] when the node exists behind the floor and
//! [`QueryOutcome::NotFound`] when it does not. A host therefore keeps its tier-refusal audit
//! while the caller learns nothing. Callers at or above the floor keep the precise answers.

use std::collections::BTreeMap;

use spot_agent_tiers::Tier;
use spot_graph_algo::{AdjacencyGraph, GraphView, Limits, reachable, shortest_path};
use spot_graph_model::NodeTypeId;

use crate::SelfModelError;
use crate::evidence::{EvidenceRef, HistoryEntry, TimeRange};
use crate::kind::{CauseDirection, EdgeKind, NodeKind};
use crate::model::{Edge, Node, NodeRef, SelfModel};

/// Who is introspecting the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The identity in the audit chain's form (`user:<id>`, `agent:<id>`).
    pub id: String,
    /// The agent tier, T0 to T3.
    pub tier: Tier,
}

impl Caller {
    /// A caller at one tier.
    #[must_use]
    pub fn new(id: impl Into<String>, tier: Tier) -> Self {
        Self {
            id: id.into(),
            tier,
        }
    }

    /// Whether the caller may see a node with this visibility floor.
    #[must_use]
    pub fn may_see(&self, node: &Node) -> bool {
        node.min_tier <= self.tier
    }
}

/// What was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKind {
    /// One node and its immediate relations.
    Describe,
    /// A node's neighbours, filtered by edge kind.
    Neighbors,
    /// A node's history in a time window.
    History,
    /// The evidence-backed causes of a change or symptom.
    Why,
}

impl QueryKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Describe => "describe",
            Self::Neighbors => "neighbors",
            Self::History => "history",
            Self::Why => "why",
        }
    }
}

/// How a query ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryOutcome {
    /// The answer was returned.
    Allowed,
    /// The caller's tier is below the node's floor.
    ///
    /// Recorded in the [`QueryRecord`] for the owner only: the caller is told the node is
    /// absent, exactly as it is for a key the snapshot does not hold (see the [`crate::service`]
    /// module docs).
    Denied,
    /// The node does not exist in this snapshot.
    NotFound,
    /// The query failed for another reason.
    Failed,
}

impl QueryOutcome {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::Failed => "failed",
        }
    }
}

/// One audited introspection query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRecord {
    /// Which query ran.
    pub query: QueryKind,
    /// Who ran it.
    pub caller: String,
    /// Their tier at the time.
    pub tier: Tier,
    /// The subject's canonical key, when there was one.
    pub subject: Option<String>,
    /// The query time supplied by the caller, in unix microseconds.
    pub at: i64,
    /// How it ended.
    pub outcome: QueryOutcome,
    /// How many items were returned (nodes, entries or paths).
    pub returned: usize,
}

/// The sink every query goes through, allowed or refused.
pub trait QueryAudit {
    /// Records one query.
    fn record(&mut self, record: QueryRecord);
}

/// An in-memory [`QueryAudit`] for tests and hosts that batch records.
#[derive(Debug, Default)]
pub struct MemoryAudit {
    records: Vec<QueryRecord>,
}

impl MemoryAudit {
    /// An empty audit log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded, in call order.
    #[must_use]
    pub fn records(&self) -> &[QueryRecord] {
        &self.records
    }
}

impl QueryAudit for MemoryAudit {
    fn record(&mut self, record: QueryRecord) {
        self.records.push(record);
    }
}

/// Which way a neighbour lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// The queried node is the edge's subject.
    Outgoing,
    /// The queried node is the edge's object.
    Incoming,
}

/// One immediate relation of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Neighbour {
    /// Which way the edge points from the queried node.
    pub direction: Direction,
    /// What the relation is.
    pub kind: EdgeKind,
    /// The node on the other end.
    pub node: Node,
    /// When the relation was recorded.
    pub at: Option<i64>,
    /// The records that prove it.
    pub evidence: Vec<EvidenceRef>,
}

/// A node and everything immediately around it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    /// The node itself.
    pub node: Node,
    /// Its immediate relations, sorted.
    pub neighbours: Vec<Neighbour>,
    /// Its history, in time order.
    pub history: Vec<HistoryEntry>,
}

/// One step of an evidence-backed cause path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CauseStep {
    /// The node at this step.
    pub node: NodeRef,
    /// What the node is.
    pub kind: NodeKind,
    /// The edge that led from the previous step to this one; `None` for the subject.
    pub via: Option<EdgeKind>,
    /// The records that prove the relation.
    pub evidence: Vec<EvidenceRef>,
    /// When the relation was recorded.
    pub at: Option<i64>,
}

/// One cause path, from the queried symptom outwards to a root cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CausePath {
    /// The steps, starting with the queried subject.
    pub steps: Vec<CauseStep>,
}

/// The answer to "why did this change?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    /// The symptom that was explained.
    pub subject: NodeRef,
    /// The snapshot epoch the answer came from.
    pub epoch: u64,
    /// The query time supplied by the caller.
    pub queried_at: i64,
    /// Evidence-backed paths from the symptom to root causes.
    pub paths: Vec<CausePath>,
}

/// How far and how wide `why` may search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhyOptions {
    /// Maximum cause hops, or `None` for no depth bound.
    pub max_depth: Option<u32>,
    /// Maximum number of root-cause paths returned.
    pub max_paths: usize,
    /// Work bounds handed to `spot-graph-algo`.
    pub limits: Limits,
}

impl Default for WhyOptions {
    fn default() -> Self {
        Self {
            max_depth: Some(8),
            max_paths: 8,
            limits: Limits::bounded_visited(10_000),
        }
    }
}

/// The service functions an MCP read tool later exposes.
pub struct SelfModelService<'a> {
    model: &'a SelfModel,
    audit: &'a mut dyn QueryAudit,
}

impl<'a> SelfModelService<'a> {
    /// A service over one snapshot and one audit sink.
    #[must_use]
    pub fn new(model: &'a SelfModel, audit: &'a mut dyn QueryAudit) -> Self {
        Self { model, audit }
    }

    /// The node this query may answer, or the one answer a caller below its floor gets.
    ///
    /// This is the crate's single visibility decision. It looks the node up, compares the
    /// caller's tier with the node's floor and, when it refuses, records the truth
    /// ([`QueryOutcome::Denied`] for a node behind the floor, [`QueryOutcome::NotFound`] for one
    /// the snapshot does not hold) on the audit sink and returns the absent-node answer.
    ///
    /// Both refusals leave through the same expression, before any traversal, history read or
    /// search, so nothing the caller can read — variant, text, serialised form, or the work done
    /// — tells them apart. Keeping the truth in the record and the answer in the error
    /// is what lets the owner audit tier refusals while the caller learns nothing.
    fn visible(
        &mut self,
        query: QueryKind,
        caller: &Caller,
        node: &NodeRef,
        at: i64,
    ) -> Result<&'a Node, SelfModelError> {
        let model: &'a SelfModel = self.model;
        match model.node_ref(node) {
            Some(found) if caller.may_see(found) => Ok(found),
            refused => {
                let outcome = if refused.is_some() {
                    QueryOutcome::Denied
                } else {
                    QueryOutcome::NotFound
                };
                self.audit(query, caller, Some(node.key()), at, outcome, 0);
                Err(SelfModelError::NodeNotFound { node: node.key() })
            }
        }
    }

    /// One node, its immediate relations and its history.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::NodeNotFound`] when the node is absent, and the same error when the
    /// caller's tier is below its floor.
    pub fn describe(
        &mut self,
        caller: &Caller,
        node: &NodeRef,
        at: i64,
    ) -> Result<Description, SelfModelError> {
        let found = self.visible(QueryKind::Describe, caller, node, at)?;
        let neighbours = self.collect_neighbours(caller, node, &[]);
        let history = self.model.history_of(&node.id()).to_vec();
        let returned = neighbours.len() + history.len();
        self.audit(
            QueryKind::Describe,
            caller,
            Some(node.key()),
            at,
            QueryOutcome::Allowed,
            returned,
        );
        Ok(Description {
            node: found.clone(),
            neighbours,
            history,
        })
    }

    /// A node's neighbours, optionally filtered to the given edge kinds (empty means all).
    ///
    /// # Errors
    ///
    /// As [`SelfModelService::describe`].
    pub fn neighbors(
        &mut self,
        caller: &Caller,
        node: &NodeRef,
        kinds: &[EdgeKind],
        at: i64,
    ) -> Result<Vec<Neighbour>, SelfModelError> {
        self.visible(QueryKind::Neighbors, caller, node, at)?;
        let neighbours = self.collect_neighbours(caller, node, kinds);
        self.audit(
            QueryKind::Neighbors,
            caller,
            Some(node.key()),
            at,
            QueryOutcome::Allowed,
            neighbours.len(),
        );
        Ok(neighbours)
    }

    /// What changed about a node inside a time window.
    ///
    /// # Errors
    ///
    /// As [`SelfModelService::describe`], plus [`SelfModelError::SourceInvalid`] for an
    /// inverted time window.
    pub fn history(
        &mut self,
        caller: &Caller,
        node: &NodeRef,
        range: &TimeRange,
        at: i64,
    ) -> Result<Vec<HistoryEntry>, SelfModelError> {
        self.visible(QueryKind::History, caller, node, at)?;
        if !range.is_valid() {
            self.audit(
                QueryKind::History,
                caller,
                Some(node.key()),
                at,
                QueryOutcome::Failed,
                0,
            );
            return Err(SelfModelError::SourceInvalid {
                source: "time_range".to_owned(),
                reason: "the lower bound is after the upper bound".to_owned(),
            });
        }
        let entries = self
            .model
            .history_between(&node.id(), range)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        self.audit(
            QueryKind::History,
            caller,
            Some(node.key()),
            at,
            QueryOutcome::Allowed,
            entries.len(),
        );
        Ok(entries)
    }

    /// The evidence-backed causes of a change or symptom, with the default options.
    ///
    /// # Errors
    ///
    /// As [`SelfModelService::describe`], plus [`SelfModelError::NoCause`] when no
    /// evidence-backed cause reaches the subject.
    pub fn why(
        &mut self,
        caller: &Caller,
        node: &NodeRef,
        at: i64,
    ) -> Result<Explanation, SelfModelError> {
        self.why_with(caller, node, &WhyOptions::default(), at)
    }

    /// The evidence-backed causes of a change or symptom.
    ///
    /// Only visible nodes and edges that carry evidence are traversed. The returned paths
    /// start at the subject and end at a root cause, so a caller reads them in the same
    /// direction as the blueprint's "field <- override <- promotion <- migration" example.
    ///
    /// # Errors
    ///
    /// As [`SelfModelService::why`], plus [`SelfModelError::LimitExceeded`] when the work
    /// bound is crossed.
    pub fn why_with(
        &mut self,
        caller: &Caller,
        node: &NodeRef,
        options: &WhyOptions,
        at: i64,
    ) -> Result<Explanation, SelfModelError> {
        let found = self.visible(QueryKind::Why, caller, node, at)?;

        let paths = match Self::why_paths(self.model, caller, found, options) {
            Ok(paths) => paths,
            Err(error) => {
                self.audit(
                    QueryKind::Why,
                    caller,
                    Some(node.key()),
                    at,
                    QueryOutcome::Failed,
                    0,
                );
                return Err(error);
            }
        };
        self.audit(
            QueryKind::Why,
            caller,
            Some(node.key()),
            at,
            QueryOutcome::Allowed,
            paths.len(),
        );
        Ok(Explanation {
            subject: node.clone(),
            epoch: self.model.epoch(),
            queried_at: at,
            paths,
        })
    }

    /// The bounded cause search over the nodes the caller may see. Pure: the caller audits.
    fn why_paths(
        model: &SelfModel,
        caller: &Caller,
        found: &Node,
        options: &WhyOptions,
    ) -> Result<Vec<CausePath>, SelfModelError> {
        let visible: Vec<&Node> = model
            .nodes()
            .filter(|candidate| caller.may_see(candidate))
            .collect();
        let index: BTreeMap<NodeId, usize> = visible
            .iter()
            .enumerate()
            .map(|(position, candidate)| (candidate.id.clone(), position))
            .collect();
        let Some(&subject) = index.get(&found.id) else {
            // Not a caller-facing tier refusal: `visible` already refused above, and the caller
            // holding `found` has been told it exists by the call that produced it. This guards
            // the search's own index instead, so it must never be reached from a query.
            return Err(SelfModelError::QueryDenied {
                node: found.reference().key(),
            });
        };

        let mut graph = AdjacencyGraph::new(visible.len())?;
        let mut cause_edges: BTreeMap<(usize, usize), Vec<Edge>> = BTreeMap::new();
        for edge in model.edges() {
            let direction = edge.kind.cause_direction();
            if direction == CauseDirection::NotCausal || edge.evidence.is_empty() {
                continue;
            }
            let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) else {
                continue;
            };
            let (effect, cause) = match direction {
                CauseDirection::EffectToCause => (from, to),
                CauseDirection::CauseToEffect => (to, from),
                CauseDirection::NotCausal => continue,
            };
            graph.add_unit_edge(to_u32(effect)?, to_u32(cause)?)?;
            cause_edges
                .entry((effect, cause))
                .or_default()
                .push(edge.clone());
        }
        for edges in cause_edges.values_mut() {
            edges.sort_by(|left, right| left.id.cmp(&right.id));
        }

        let subject_ix = to_u32(subject)?;
        let reached = reachable(&graph, &[subject_ix], options.max_depth, &options.limits)?;
        if reached.len() <= 1 {
            return Err(SelfModelError::NoCause {
                subject: found.reference().key(),
            });
        }

        let mut leaves: Vec<usize> = reached
            .iter()
            .filter_map(|&position| usize::try_from(position).ok())
            .filter(|&position| position != subject)
            .filter(|&position| {
                u32::try_from(position).is_ok_and(|index| {
                    graph
                        .successors(index)
                        .all(|(target, _)| !reached.contains(&target))
                })
            })
            .collect();
        leaves.sort_by_key(|&position| visible[position].key.clone());

        let mut paths = Vec::new();
        for leaf in leaves {
            if paths.len() >= options.max_paths {
                break;
            }
            let Ok(leaf_ix) = u32::try_from(leaf) else {
                continue;
            };
            let Some(path) = shortest_path(&graph, subject_ix, leaf_ix, &options.limits)? else {
                continue;
            };
            let mut steps = Vec::with_capacity(path.nodes.len());
            steps.push(CauseStep {
                node: visible[subject].reference(),
                kind: visible[subject].kind,
                via: None,
                evidence: visible[subject].evidence.clone(),
                at: None,
            });
            for pair in path.nodes.windows(2) {
                let (Ok(left), Ok(right)) = (usize::try_from(pair[0]), usize::try_from(pair[1]))
                else {
                    continue;
                };
                let Some(edges) = cause_edges.get(&(left, right)) else {
                    continue;
                };
                let Some(edge) = edges.first() else {
                    continue;
                };
                steps.push(CauseStep {
                    node: visible[right].reference(),
                    kind: visible[right].kind,
                    via: Some(edge.kind),
                    evidence: edge.evidence.clone(),
                    at: edge.at,
                });
            }
            if steps.len() > 1 {
                paths.push(CausePath { steps });
            }
        }

        if paths.is_empty() {
            return Err(SelfModelError::NoCause {
                subject: found.reference().key(),
            });
        }
        Ok(paths)
    }

    fn collect_neighbours(
        &self,
        caller: &Caller,
        node: &NodeRef,
        kinds: &[EdgeKind],
    ) -> Vec<Neighbour> {
        let id = node.id();
        let mut out = Vec::new();
        for edge in self.model.touching(&id) {
            if !kinds.is_empty() && !kinds.contains(&edge.kind) {
                continue;
            }
            if edge.from == id
                && let Some(other) = self.model.node(&edge.to)
                && caller.may_see(other)
            {
                out.push(Neighbour {
                    direction: Direction::Outgoing,
                    kind: edge.kind,
                    node: other.clone(),
                    at: edge.at,
                    evidence: edge.evidence.clone(),
                });
            }
            if edge.to == id
                && let Some(other) = self.model.node(&edge.from)
                && caller.may_see(other)
            {
                out.push(Neighbour {
                    direction: Direction::Incoming,
                    kind: edge.kind,
                    node: other.clone(),
                    at: edge.at,
                    evidence: edge.evidence.clone(),
                });
            }
        }
        out.sort_by(|left, right| {
            left.direction
                .cmp(&right.direction)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.node.key.cmp(&right.node.key))
        });
        out
    }

    fn audit(
        &mut self,
        query: QueryKind,
        caller: &Caller,
        subject: Option<String>,
        at: i64,
        outcome: QueryOutcome,
        returned: usize,
    ) {
        self.audit.record(QueryRecord {
            query,
            caller: caller.id.clone(),
            tier: caller.tier,
            subject,
            at,
            outcome,
            returned,
        });
    }
}

fn to_u32(value: usize) -> Result<u32, SelfModelError> {
    u32::try_from(value).map_err(|_| SelfModelError::SourceInvalid {
        source: "why".to_owned(),
        reason: "the visible node index does not fit a graph node".to_owned(),
    })
}

// A short alias keeps the public types readable and the map key exact.
type NodeId = NodeTypeId;
