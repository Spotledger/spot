//! The held region: which nodes' external effects wait for which commit point (ADR-080 6.7).
//!
//! A node's external effects are held **iff** the node lies in a region that ends in a commit
//! point. The region of a commit point is every node from which it can be reached without passing
//! through another commit point; the commit point itself and anything after the last one are
//! outside. A process with no commit point holds nothing, so existing behaviour is unchanged.
//!
//! The analysis is pure: it reads the compiled graph, the commit nodes, the effect classes each
//! node declares (from the seam registry) and the policy table, and produces a [`RegionPlan`]
//! plus the design-time [`Finding`]s `validate` reports.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spot_dag::{CompiledDag, NodeId};
use spot_effect_hold::{EffectClass, PolicyTable};

use crate::error::{CommitError, Result};

/// The most effect classes one node may declare.
pub const MAX_CLASSES_PER_NODE: usize = 64;

/// The most nodes a process may have for region analysis.
pub const MAX_NODES: usize = 10_000;
/// The most commit points one process may mark.
pub const MAX_COMMIT_POINTS: usize = 256;

/// The effect classes each node declares, by node.
pub type NodeEffects = BTreeMap<NodeId, BTreeSet<EffectClass>>;

/// How serious a finding is. Both kinds are reported to the designer; neither blocks a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth the designer's attention before going live.
    Warning,
}

/// What `validate` reports about commit points.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    /// A commit point whose region declares nothing that can be held.
    NothingToHold,
    /// A node in a held region causes an effect of a never-held class (fiscal transmission): the
    /// effect leaves at once and cannot be discarded, so undo stops here.
    PointOfNoReturn {
        /// The class key.
        class: String,
    },
}

/// One design-time finding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The node the finding is about: the commit point, or the node causing the effect.
    pub node: NodeId,
    /// What was found.
    pub kind: FindingKind,
    /// How serious it is.
    pub severity: Severity,
}

impl Finding {
    /// The stable token of the finding, for the designer and for tests.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self.kind {
            FindingKind::NothingToHold => "commit_point_holds_nothing",
            FindingKind::PointOfNoReturn { .. } => "point_of_no_return",
        }
    }
}

/// The ordering source an effect produced at `node` is recorded under, so a commit point can
/// release exactly the effects of its region.
#[must_use]
pub fn node_source(node: &NodeId) -> String {
    format!("node:{}", node.as_str())
}

/// The held regions of one process.
#[derive(Clone, Debug, Default)]
pub struct RegionPlan {
    regions: BTreeMap<NodeId, BTreeSet<NodeId>>,
    findings: Vec<Finding>,
}

impl RegionPlan {
    /// A plan that holds nothing: a process with no commit point.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Analyses `dag` for the given commit points.
    ///
    /// # Errors
    /// [`CommitError::InvalidDeclaration`] when a commit point or a declaring node is not in the
    /// graph, a commit point declares effects itself, a node declares more than
    /// [`MAX_CLASSES_PER_NODE`] classes, or the process has more than [`MAX_NODES`] nodes or
    /// [`MAX_COMMIT_POINTS`] commit points.
    pub fn analyze(
        dag: &CompiledDag,
        commits: &BTreeSet<NodeId>,
        effects: &NodeEffects,
        policies: &PolicyTable,
    ) -> Result<Self> {
        let invalid = |detail: &str| CommitError::InvalidDeclaration {
            detail: detail.to_owned(),
        };
        if dag.nodes().len() > MAX_NODES {
            return Err(invalid("the process has too many nodes to analyse"));
        }
        if commits.len() > MAX_COMMIT_POINTS {
            return Err(invalid("the process marks too many commit points"));
        }
        for commit in commits {
            if dag.node(commit).is_none() {
                return Err(invalid("a commit point is not a node of the process"));
            }
        }
        for (node, classes) in effects {
            if dag.node(node).is_none() {
                return Err(invalid("effects are declared for a node the process lacks"));
            }
            if commits.contains(node) && !classes.is_empty() {
                return Err(invalid("a commit point cannot cause effects itself"));
            }
            if classes.len() > MAX_CLASSES_PER_NODE {
                return Err(invalid("a node declares too many effect classes"));
            }
        }

        let mut regions = BTreeMap::new();
        for commit in commits {
            regions.insert(commit.clone(), region_of(dag, commit, commits));
        }

        let mut findings = Vec::new();
        for (commit, region) in &regions {
            let holdable = region.iter().any(|node| {
                effects.get(node).is_some_and(|classes| {
                    classes.iter().any(|class| !policies.is_never_hold(class))
                })
            });
            if !holdable {
                findings.push(Finding {
                    node: commit.clone(),
                    kind: FindingKind::NothingToHold,
                    severity: Severity::Warning,
                });
            }
        }
        let held: BTreeSet<&NodeId> = regions.values().flatten().collect();
        for (node, classes) in effects {
            if !held.contains(node) {
                continue;
            }
            for class in classes.iter().filter(|c| policies.is_never_hold(c)) {
                findings.push(Finding {
                    node: node.clone(),
                    kind: FindingKind::PointOfNoReturn { class: class.key() },
                    severity: Severity::Warning,
                });
            }
        }
        findings.sort();
        Ok(Self { regions, findings })
    }

    /// Whether `node` is a commit point of this plan.
    #[must_use]
    pub fn is_commit_point(&self, node: &NodeId) -> bool {
        self.regions.contains_key(node)
    }

    /// Whether the external effects of `node` are held: it lies in some region.
    #[must_use]
    pub fn is_held(&self, node: &NodeId) -> bool {
        self.regions.values().any(|region| region.contains(node))
    }

    /// The nodes of a commit point's region, if it is one.
    #[must_use]
    pub fn region(&self, commit: &NodeId) -> Option<&BTreeSet<NodeId>> {
        self.regions.get(commit)
    }

    /// The ordering sources a commit point releases: those of its region's nodes.
    #[must_use]
    pub fn sources(&self, commit: &NodeId) -> Option<BTreeSet<String>> {
        self.regions
            .get(commit)
            .map(|region| region.iter().map(node_source).collect())
    }

    /// The design-time findings, in a stable order.
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// The points of no return: nodes whose never-held effect leaves from inside a held region.
    #[must_use]
    pub fn points_of_no_return(&self) -> Vec<&NodeId> {
        self.findings
            .iter()
            .filter(|f| matches!(f.kind, FindingKind::PointOfNoReturn { .. }))
            .map(|f| &f.node)
            .collect()
    }
}

/// Every node from which `commit` is reachable without passing through another commit point.
fn region_of(dag: &CompiledDag, commit: &NodeId, commits: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
    let mut region = BTreeSet::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    queue.push_back(commit.clone());
    while let Some(current) = queue.pop_front() {
        for edge in dag.predecessors(&current) {
            if commits.contains(&edge.node) || !region.insert(edge.node.clone()) {
                continue;
            }
            queue.push_back(edge.node.clone());
        }
    }
    region
}
