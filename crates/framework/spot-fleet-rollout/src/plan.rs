//! The rollout plan: canary waves, their selectors, sizes, soak, success criteria, the pause
//! policy and the version-skew limit; and the deterministic resolution of a fleet into waves.
//!
//! A [`RolloutPlan`] is data. [`RolloutPlan::resolve`] turns it into a concrete schedule of node
//! ids for one fleet and one [`Seed`], so two callers with the plan, fleet and seed always get
//! the same order. A node belongs to at most one wave.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::epoch::{Epoch, SkewLimit};
use crate::error::Error;
use crate::ids::{NodeId, Percent, ReleaseId, Seed, Soak, SplitMix64, wave_seed};

/// The health of a node, matching the one health surface's three colours (ADR-062 rule 4).
///
/// The order is the report order: [`Health::Red`] is worst and [`Health::Green`] is best, so a
/// criterion can say "at least amber" with `health >= Health::Amber`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// A person is needed.
    Red,
    /// The system is handling it.
    Amber,
    /// Healthy.
    Green,
}

/// What a wave must observe before the rollout may move on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessCriteria {
    /// Every node in the wave must have finished verify.
    pub require_verified: bool,
    /// The highest error rate, in integer permille, a node may report. `1000` allows everything.
    pub max_error_rate_permille: u32,
    /// The worst health a node may report and still pass.
    pub min_health: Health,
}

impl Default for SuccessCriteria {
    fn default() -> Self {
        Self {
            require_verified: true,
            max_error_rate_permille: 10,
            min_health: Health::Green,
        }
    }
}

impl SuccessCriteria {
    /// Every node verified, no errors, all green.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            require_verified: true,
            max_error_rate_permille: 0,
            min_health: Health::Green,
        }
    }
}

/// The fleet-wide conditions that pause the rollout even outside the active wave's criteria.
///
/// A failure on a canary node pauses everything regardless of this policy; the canary rule is
/// not configurable (ADR-22: a canary failure halts the fleet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseOn {
    /// Pause when any node reports a failure.
    pub any_failure: bool,
    /// Pause when any observed node's error rate exceeds this many permille.
    pub max_error_rate_permille: u32,
    /// Pause when any observed node is worse than this health.
    pub min_health: Health,
}

impl Default for PauseOn {
    fn default() -> Self {
        Self {
            any_failure: true,
            max_error_rate_permille: 20,
            min_health: Health::Amber,
        }
    }
}

impl PauseOn {
    /// Fail on any failure, any error and anything but green.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            any_failure: true,
            max_error_rate_permille: 0,
            min_health: Health::Green,
        }
    }
}

/// How a selector chooses from the nodes an earlier wave did not take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    /// Every listed tag must be present on the node.
    pub tags: Vec<String>,
    /// The node must be in one of the listed groups; empty means any group.
    pub groups: Vec<String>,
    /// The percentage of the matching pool to take, before [`WaveSize`].
    pub percentage: Percent,
}

impl Default for Selector {
    fn default() -> Self {
        Self::all()
    }
}

impl Selector {
    /// Every remaining node.
    #[must_use]
    pub fn all() -> Self {
        Self {
            tags: Vec::new(),
            groups: Vec::new(),
            percentage: Percent::ALL,
        }
    }

    /// Require every one of `tags`.
    #[must_use]
    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Require membership of one of `groups`.
    #[must_use]
    pub fn with_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups = groups.into_iter().map(Into::into).collect();
        self
    }

    /// Take `percentage` of the matching pool.
    #[must_use]
    pub fn with_percentage(mut self, percentage: Percent) -> Self {
        self.percentage = percentage;
        self
    }

    /// Whether `node` matches the tag and group filters (percentage is applied after this).
    #[must_use]
    pub fn matches(&self, node: &FleetNode) -> bool {
        self.tags.iter().all(|tag| node.tags.contains(tag))
            && (self.groups.is_empty() || self.groups.contains(&node.group))
    }
}

/// The most nodes a wave may take, applied after the selector's percentage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveSize {
    /// Every node the selector kept.
    #[default]
    All,
    /// At most this many nodes.
    Nodes(u32),
    /// This percentage of the nodes the selector kept.
    Percent(Percent),
}

/// Whether a wave may start before its predecessor finished soaking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    /// The default: the next wave starts only after the previous one passed and soaked.
    #[default]
    None,
    /// This wave may start as soon as the previous one started, without waiting for its soak.
    WithPrevious,
}

/// One wave of a rollout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wave {
    /// A name unique in the plan, used in actions and reports.
    pub name: String,
    /// Which nodes the wave takes.
    pub selector: Selector,
    /// How many of them.
    pub size: WaveSize,
    /// How long the wave must stay healthy before the next one may start.
    pub soak: Soak,
    /// What the wave must observe to pass.
    pub success_criteria: SuccessCriteria,
    /// Whether this is the canary wave. Exactly one wave, the first, is the canary.
    pub canary: bool,
    /// Whether this wave may overlap its predecessor.
    pub overlap: Overlap,
}

impl Wave {
    /// A wave that does not overlap and is not the canary.
    #[must_use]
    pub fn new(name: impl Into<String>, selector: Selector, size: WaveSize, soak: Soak) -> Self {
        Self {
            name: name.into(),
            selector,
            size,
            soak,
            success_criteria: SuccessCriteria::default(),
            canary: false,
            overlap: Overlap::None,
        }
    }

    /// Mark this wave as the canary.
    #[must_use]
    pub fn as_canary(mut self) -> Self {
        self.canary = true;
        self
    }

    /// Let this wave start as soon as its predecessor started.
    #[must_use]
    pub fn overlapping(mut self) -> Self {
        self.overlap = Overlap::WithPrevious;
        self
    }

    /// Replace the success criteria.
    #[must_use]
    pub fn with_criteria(mut self, criteria: SuccessCriteria) -> Self {
        self.success_criteria = criteria;
        self
    }
}

/// The maximum number of node migrations the apply engine dispatches in one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConcurrencyLimit(NonZeroUsize);

impl ConcurrencyLimit {
    /// A limit of at least one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPlan`] when `limit` is zero.
    pub fn new(limit: usize) -> Result<Self, Error> {
        NonZeroUsize::new(limit)
            .map(Self)
            .ok_or_else(|| Error::InvalidPlan {
                reason: "the concurrency limit is zero".to_owned(),
            })
    }

    /// The limit.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for ConcurrencyLimit {
    fn default() -> Self {
        Self(NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN))
    }
}

/// The whole rollout: the release, the epoch it verifies nodes at, the waves in start order and
/// the policy that governs pauses and sync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutPlan {
    /// The release being applied.
    pub release: ReleaseId,
    /// The migration epoch this release assigns to every node that finishes verify.
    pub epoch: Epoch,
    /// The waves, in start order. The first is the canary.
    pub waves: Vec<Wave>,
    /// The fleet-wide pause conditions.
    pub pause_on: PauseOn,
    /// How far behind the hub an edge may be and still sync after this rollout.
    pub skew_limit: SkewLimit,
    /// How many node migrations may be in flight at once.
    pub concurrency: ConcurrencyLimit,
}

impl RolloutPlan {
    /// A plan with the default pause policy, a strict skew limit and the default concurrency.
    #[must_use]
    pub fn new(release: ReleaseId, epoch: Epoch, waves: Vec<Wave>) -> Self {
        Self {
            release,
            epoch,
            waves,
            pause_on: PauseOn::default(),
            skew_limit: SkewLimit::STRICT,
            concurrency: ConcurrencyLimit::default(),
        }
    }

    /// Checks every structural rule.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPlan`] naming the first rule the plan breaks.
    pub fn validate(&self) -> Result<(), Error> {
        if self.waves.is_empty() {
            return Err(Error::InvalidPlan {
                reason: "the plan has no waves".to_owned(),
            });
        }
        let mut names = BTreeSet::new();
        let mut canaries = 0_u32;
        for (index, wave) in self.waves.iter().enumerate() {
            if wave.name.is_empty() {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {index} has an empty name"),
                });
            }
            if !names.insert(wave.name.clone()) {
                return Err(Error::InvalidPlan {
                    reason: format!("wave name {} is used twice", wave.name),
                });
            }
            if wave.canary {
                canaries = canaries.saturating_add(1);
                if index != 0 {
                    return Err(Error::InvalidPlan {
                        reason: "the canary must be the first wave".to_owned(),
                    });
                }
            }
            if index == 0 && !wave.canary {
                return Err(Error::InvalidPlan {
                    reason: "the first wave must be the canary".to_owned(),
                });
            }
            if index == 0 && wave.overlap != Overlap::None {
                return Err(Error::InvalidPlan {
                    reason: "the first wave cannot overlap a predecessor".to_owned(),
                });
            }
            if matches!(wave.size, WaveSize::Nodes(0)) {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {} allows zero nodes", wave.name),
                });
            }
            if matches!(wave.size, WaveSize::Percent(percent) if percent.get() == 0) {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {} takes zero percent of its pool", wave.name),
                });
            }
            if wave.selector.percentage.get() == 0 {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {} selects zero percent of its pool", wave.name),
                });
            }
            if wave.selector.tags.iter().any(String::is_empty)
                || wave.selector.groups.iter().any(String::is_empty)
            {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {} has an empty tag or group", wave.name),
                });
            }
            if wave.success_criteria.max_error_rate_permille > 1_000 {
                return Err(Error::InvalidPlan {
                    reason: format!("wave {} has an error rate above 1000 permille", wave.name),
                });
            }
        }
        if canaries != 1 {
            return Err(Error::InvalidPlan {
                reason: "exactly one wave must be the canary".to_owned(),
            });
        }
        if self.pause_on.max_error_rate_permille > 1_000 {
            return Err(Error::InvalidPlan {
                reason: "the pause policy has an error rate above 1000 permille".to_owned(),
            });
        }
        Ok(())
    }

    /// Resolves the plan against `fleet` for `seed`.
    ///
    /// Nodes are considered in id order, shuffled deterministically per wave, then cut by the
    /// selector's percentage and the wave size. A node taken by one wave is never in a later
    /// one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPlan`] when the plan is invalid or the fleet repeats a node id, and
    /// [`Error::EmptyWave`] when a wave has no node left.
    pub fn resolve(&self, fleet: &Fleet, seed: Seed) -> Result<Vec<ResolvedWave>, Error> {
        self.validate()?;
        let mut fleet_order: Vec<&FleetNode> = fleet.nodes().iter().collect();
        fleet_order.sort_by(|left, right| left.id.cmp(&right.id));
        let mut seen = BTreeSet::new();
        for node in &fleet_order {
            if !seen.insert(node.id.clone()) {
                return Err(Error::InvalidPlan {
                    reason: format!("the fleet repeats node {}", node.id),
                });
            }
        }

        let mut claimed = BTreeSet::new();
        let mut resolved = Vec::with_capacity(self.waves.len());
        for (index, wave) in self.waves.iter().enumerate() {
            let mut pool: Vec<NodeId> = fleet_order
                .iter()
                .filter(|node| !claimed.contains(&node.id) && wave.selector.matches(node))
                .map(|node| node.id.clone())
                .collect();
            let mut generator = SplitMix64::new(wave_seed(seed, index));
            generator.shuffle(&mut pool);
            let mut selected = take_percent(&pool, wave.selector.percentage);
            selected = match wave.size {
                WaveSize::All => selected,
                WaveSize::Nodes(limit) => {
                    selected.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
                    selected
                }
                WaveSize::Percent(percent) => take_percent(&selected, percent),
            };
            if selected.is_empty() {
                return Err(Error::EmptyWave {
                    wave: wave.name.clone(),
                });
            }
            claimed.extend(selected.iter().cloned());
            resolved.push(ResolvedWave {
                index,
                name: wave.name.clone(),
                canary: wave.canary,
                nodes: selected,
            });
        }
        Ok(resolved)
    }
}

/// One wave resolved to concrete nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedWave {
    /// The wave's index in the plan.
    pub index: usize,
    /// The wave's name.
    pub name: String,
    /// Whether this is the canary.
    pub canary: bool,
    /// The nodes in start order.
    pub nodes: Vec<NodeId>,
}

/// A node in the fleet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNode {
    /// The node identity.
    pub id: NodeId,
    /// The node's group, for example a region or a branch cluster.
    pub group: String,
    /// The node's tags, for example `retail` or `warehouse`.
    pub tags: BTreeSet<String>,
}

impl FleetNode {
    /// A node in `group` with no tags.
    #[must_use]
    pub fn new(id: NodeId, group: impl Into<String>) -> Self {
        Self {
            id,
            group: group.into(),
            tags: BTreeSet::new(),
        }
    }

    /// Add a tag.
    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.insert(tag.into());
        self
    }
}

/// The fleet a rollout is planned against.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    nodes: Vec<FleetNode>,
}

impl Fleet {
    /// A fleet.
    #[must_use]
    pub fn new(nodes: Vec<FleetNode>) -> Self {
        Self { nodes }
    }

    /// The nodes.
    #[must_use]
    pub fn nodes(&self) -> &[FleetNode] {
        &self.nodes
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the fleet has no node.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node with `id`.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&FleetNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    /// A map from node id to node, for the state machine's per-tick lookups.
    #[must_use]
    pub fn by_id(&self) -> BTreeMap<&NodeId, &FleetNode> {
        self.nodes.iter().map(|node| (&node.id, node)).collect()
    }
}

/// The first `percent` of `items`, rounding a non-empty pool up to one.
fn take_percent(items: &[NodeId], percent: Percent) -> Vec<NodeId> {
    if items.is_empty() {
        return Vec::new();
    }
    let count = items
        .len()
        .saturating_mul(usize::from(percent.get()))
        .saturating_div(100);
    let count = count.max(usize::from(percent.get() > 0));
    items.iter().take(count).cloned().collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn node(name: &str, group: &str) -> FleetNode {
        FleetNode::new(NodeId::new(name).expect("a valid test node"), group)
    }

    fn release() -> ReleaseId {
        ReleaseId::new("2026.10.1").expect("a valid release")
    }

    #[test]
    fn resolution_is_deterministic_and_partitions_the_fleet() {
        let fleet = Fleet::new(
            (0..40)
                .map(|index| node(&format!("n{index:02}"), "g"))
                .collect(),
        );
        let plan = RolloutPlan::new(
            release(),
            Epoch::new(1),
            vec![
                Wave::new(
                    "canary",
                    Selector::all(),
                    WaveSize::Nodes(4),
                    Soak::from_secs(1),
                )
                .as_canary(),
                Wave::new(
                    "middle",
                    Selector::all(),
                    WaveSize::Percent(Percent::new(50).expect("valid")),
                    Soak::from_secs(1),
                ),
                Wave::new("tail", Selector::all(), WaveSize::All, Soak::ZERO),
            ],
        );
        let first = plan.resolve(&fleet, Seed::new(7)).expect("resolve");
        let second = plan.resolve(&fleet, Seed::new(7)).expect("resolve");
        assert_eq!(first, second);
        let all: BTreeSet<NodeId> = first
            .iter()
            .flat_map(|wave| wave.nodes.iter().cloned())
            .collect();
        assert_eq!(all.len(), 40, "every node belongs to exactly one wave");
        assert_eq!(first[0].nodes.len(), 4);
    }

    #[test]
    fn selectors_filter_by_tag_group_and_percentage() {
        let fleet = Fleet::new(vec![
            node("a", "north").with_tag("retail"),
            node("b", "north").with_tag("retail"),
            node("c", "south").with_tag("retail"),
            node("d", "south").with_tag("warehouse"),
            node("e", "south").with_tag("warehouse"),
        ]);
        let selector = Selector::all()
            .with_tags(["retail"])
            .with_groups(["south"])
            .with_percentage(Percent::new(100).expect("valid"));
        let plan = RolloutPlan::new(
            release(),
            Epoch::new(1),
            vec![Wave::new("canary", selector, WaveSize::All, Soak::ZERO).as_canary()],
        );
        let resolved = plan.resolve(&fleet, Seed::new(1)).expect("resolve");
        assert_eq!(resolved[0].nodes, vec![NodeId::new("c").expect("valid")]);
    }

    #[test]
    fn duplicate_fleet_ids_are_refused() {
        let fleet = Fleet::new(vec![node("a", "g"), node("a", "g")]);
        let plan = RolloutPlan::new(
            release(),
            Epoch::new(1),
            vec![Wave::new("canary", Selector::all(), WaveSize::All, Soak::ZERO).as_canary()],
        );
        assert_eq!(
            plan.resolve(&fleet, Seed::new(1)),
            Err(Error::InvalidPlan {
                reason: "the fleet repeats node a".to_owned()
            })
        );
    }

    #[test]
    fn invalid_plans_are_refused() {
        let empty = RolloutPlan::new(release(), Epoch::new(1), Vec::new());
        assert!(empty.validate().is_err());
        let no_canary = RolloutPlan::new(
            release(),
            Epoch::new(1),
            vec![Wave::new(
                "wave",
                Selector::all(),
                WaveSize::All,
                Soak::ZERO,
            )],
        );
        assert!(no_canary.validate().is_err());
        let late_canary = RolloutPlan::new(
            release(),
            Epoch::new(1),
            vec![
                Wave::new("first", Selector::all(), WaveSize::Nodes(1), Soak::ZERO).as_canary(),
                Wave::new("second", Selector::all(), WaveSize::Nodes(1), Soak::ZERO).as_canary(),
            ],
        );
        assert!(late_canary.validate().is_err());
    }
}
