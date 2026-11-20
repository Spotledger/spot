//! The soak plan: every disturbance of every simulated day, drawn from one seed.
//!
//! A plan is data. [`SoakPlan::generate`] draws it from the seed with the crate's own generator
//! ([`spot_sync_chaos::Rng`], the one seeded generator of the suite) and nothing else: no thread
//! RNG, no clock. Two plans from one seed are equal, which is what lets a failing seed replay.
//!
//! Time is counted in **rounds** of the world (a day is `rounds_per_day` rounds). A round is
//! `round_minutes()` virtual minutes, so an outage of "1 minute to 36 hours" is rounded up to
//! whole rounds, never down to zero.

use std::collections::BTreeSet;

use spot_sync_chaos::{Edge, Rng, Step};

use super::release::ReleaseSchedule;

/// Minutes in a day.
pub const MINUTES_PER_DAY: u64 = 24 * 60;
/// The longest outage, in minutes (36 hours).
pub const MAX_OUTAGE_MINUTES: u64 = 36 * 60;
/// The largest clock jump, in milliseconds (ten minutes).
pub const MAX_JUMP_MS: u64 = 10 * 60 * 1_000;

/// How big a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// 3 days, 10 nodes: runs in the default test run.
    Smoke,
    /// 30 days, 100 nodes: the acceptance run.
    Full,
}

impl Shape {
    /// The shape's name, as the replay line prints it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Full => "full",
        }
    }

    /// The shape named `name`, when there is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "smoke" => Some(Self::Smoke),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Everything a soak run is a function of, besides the seed's draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoakSettings {
    /// The seed.
    pub seed: u64,
    /// The shape this came from (for the replay line).
    pub shape: Shape,
    /// Simulated days.
    pub days: u32,
    /// Nodes.
    pub nodes: usize,
    /// Rounds in one simulated day.
    pub rounds_per_day: u32,
    /// Records the hub writes per wave (one wave per round).
    pub records: usize,
    /// The first day nodes upgrade to release N+1 (the hub upgrades at its start).
    pub release_start_day: u32,
    /// The last day a node upgrades on.
    pub release_end_day: u32,
    /// The most nodes down at once, in parts per thousand of the fleet (at least one node).
    pub max_down_permille: u64,
}

impl SoakSettings {
    /// The short variant: 3 days, 10 nodes.
    #[must_use]
    pub fn smoke(seed: u64) -> Self {
        Self {
            seed,
            shape: Shape::Smoke,
            days: 3,
            nodes: 10,
            rounds_per_day: 24,
            records: 6,
            release_start_day: 1,
            release_end_day: 2,
            max_down_permille: 400,
        }
    }

    /// The acceptance variant: 30 days, 100 nodes, the rollout from day 10 to day 20.
    #[must_use]
    pub fn full(seed: u64) -> Self {
        Self {
            seed,
            shape: Shape::Full,
            days: 30,
            nodes: 100,
            rounds_per_day: 48,
            records: 6,
            release_start_day: 10,
            release_end_day: 20,
            max_down_permille: 300,
        }
    }

    /// The settings of `shape` for `seed`.
    #[must_use]
    pub fn of(shape: Shape, seed: u64) -> Self {
        match shape {
            Shape::Smoke => Self::smoke(seed),
            Shape::Full => Self::full(seed),
        }
    }

    /// Rounds in the whole run.
    #[must_use]
    pub fn total_rounds(&self) -> u64 {
        u64::from(self.days) * u64::from(self.rounds_per_day)
    }

    /// Virtual minutes one round advances.
    #[must_use]
    pub fn round_minutes(&self) -> u64 {
        (MINUTES_PER_DAY / u64::from(self.rounds_per_day.max(1))).max(1)
    }

    /// Virtual milliseconds one round advances (the world's `tick_ms`).
    #[must_use]
    pub fn tick_ms(&self) -> u64 {
        self.round_minutes() * 60_000
    }

    /// The most nodes that may be down in one round.
    #[must_use]
    pub fn max_down(&self) -> usize {
        let nodes = u64::try_from(self.nodes).unwrap_or(u64::MAX);
        let cap = nodes.saturating_mul(self.max_down_permille) / 1_000;
        usize::try_from(cap.max(1)).unwrap_or(1)
    }
}

/// One disturbance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disturbance {
    /// The node is unreachable for `rounds` rounds from the moment of the event.
    Outage {
        /// The node.
        node: usize,
        /// How many rounds.
        rounds: u64,
    },
    /// The link cuts at several protocol steps in one round and is healed the next.
    Flap {
        /// The node.
        node: usize,
        /// The cuts armed, one per step at most.
        cuts: Vec<(Step, Edge)>,
    },
    /// The node is restored from a backup older than its last acknowledgement and starts a
    /// session (the hub must detect it and bootstrap it).
    Restore {
        /// The node.
        node: usize,
    },
    /// The node's wall clock jumps, the hub's does not.
    ClockJump {
        /// The node.
        node: usize,
        /// Milliseconds, positive or negative.
        delta_ms: i64,
    },
    /// The hub restarts and loses nothing durable.
    HubRestart,
}

impl Disturbance {
    /// One line for the trace and the plan dump: kinds, nodes and sizes only.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Outage { node, rounds } => format!("outage node-{node} for {rounds} rounds"),
            Self::Flap { node, cuts } => {
                let cuts: Vec<String> = cuts
                    .iter()
                    .map(|(step, edge)| format!("{}:{}", step.name(), edge.name()))
                    .collect();
                format!("flap node-{node} cuts [{}]", cuts.join(" "))
            }
            Self::Restore { node } => format!("restore node-{node} from backup"),
            Self::ClockJump { node, delta_ms } => format!("clock jump node-{node} {delta_ms} ms"),
            Self::HubRestart => "hub restart".to_owned(),
        }
    }
}

/// A disturbance at a virtual time: a day and a round within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// The day, from 0.
    pub day: u32,
    /// The round within the day, from 0.
    pub round: u32,
    /// What happens.
    pub what: Disturbance,
}

/// The whole plan of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoakPlan {
    /// The settings it was drawn for.
    pub settings: SoakSettings,
    /// The rolling upgrade.
    pub schedule: ReleaseSchedule,
    /// The disturbances, sorted by day then round (ties keep their drawing order).
    pub events: Vec<Planned>,
}

/// Rounds an outage of `minutes` takes: whole rounds, at least one.
fn rounds_for(minutes: u64, round_minutes: u64) -> u64 {
    minutes.div_ceil(round_minutes.max(1)).max(1)
}

impl SoakPlan {
    /// Draws the plan of `settings`.
    #[must_use]
    pub fn generate(settings: &SoakSettings) -> Self {
        let mut rng = Rng::new(settings.seed ^ 0x50AC_0000_5EED_0001);
        let schedule = ReleaseSchedule::generate(settings, &mut rng);
        let rounds_per_day = u64::from(settings.rounds_per_day.max(1));
        let total = settings.total_rounds();
        let round_minutes = settings.round_minutes();
        let cap = settings.max_down();
        // down[node][round]: planned unreachable. down_count[round]: how many nodes.
        let width = usize::try_from(total).unwrap_or(0);
        let mut down: Vec<Vec<bool>> = vec![vec![false; width]; settings.nodes];
        let mut down_count: Vec<usize> = vec![0; width];
        let mut events: Vec<Planned> = Vec::new();

        for day in 0..settings.days {
            // Outages: 15% of the nodes a day, on average.
            for (node, node_down) in down.iter_mut().enumerate() {
                if !rng.chance(150) {
                    continue;
                }
                let round_in_day = rng.below(rounds_per_day);
                let minutes = 1 + rng.below(MAX_OUTAGE_MINUTES);
                let start = u64::from(day) * rounds_per_day + round_in_day;
                // Every outage heals before the last day ends.
                let end = (start + rounds_for(minutes, round_minutes)).min(total);
                let range = usize::try_from(start).unwrap_or(0)..usize::try_from(end).unwrap_or(0);
                let blocked = range.clone().any(|r| node_down[r] || down_count[r] >= cap);
                if blocked || range.is_empty() {
                    continue;
                }
                for r in range {
                    node_down[r] = true;
                    down_count[r] += 1;
                }
                events.push(Planned {
                    day,
                    round: u32::try_from(round_in_day).unwrap_or(0),
                    what: Disturbance::Outage {
                        node,
                        rounds: end - start,
                    },
                });
            }
            // Link flaps: about one for every ten nodes a day.
            let flaps = rng.below(2 * u64::try_from(settings.nodes / 10).unwrap_or(0).max(1));
            for _ in 0..flaps {
                let node = pick_node(&mut rng, settings.nodes);
                let round = u32::try_from(rng.below(rounds_per_day)).unwrap_or(0);
                let mut cuts: Vec<(Step, Edge)> = Vec::new();
                for step in Step::all() {
                    if rng.chance(500) {
                        let edge = if rng.chance(500) {
                            Edge::Before
                        } else {
                            Edge::After
                        };
                        cuts.push((step, edge));
                    }
                }
                if cuts.is_empty() {
                    cuts.push((Step::Offer, Edge::Before));
                }
                events.push(Planned {
                    day,
                    round,
                    what: Disturbance::Flap { node, cuts },
                });
            }
            // A node restored from a backup: one every three days, on average. It is never a
            // node that is down at that moment and never one that upgrades that day: a restore
            // rewrites the node's state (bootstrap) and an upgrade rewrites what the hub holds
            // for it (release), and doing both in one day would make a violation impossible to
            // attribute to either repair path.
            if rng.chance(333) {
                let round = u32::try_from(rng.below(rounds_per_day)).unwrap_or(0);
                let abs = usize::try_from(u64::from(day) * rounds_per_day + u64::from(round))
                    .unwrap_or(0);
                for _ in 0..8 {
                    let node = pick_node(&mut rng, settings.nodes);
                    let is_down = down[node].get(abs).copied().unwrap_or(true);
                    if is_down || schedule.upgrade_day[node] == Some(day) {
                        continue;
                    }
                    events.push(Planned {
                        day,
                        round,
                        what: Disturbance::Restore { node },
                    });
                    break;
                }
            }
            // Clock jumps: none, one or two a day, one on average, up to ten minutes either way.
            for _ in 0..rng.below(3) {
                let node = pick_node(&mut rng, settings.nodes);
                let round = u32::try_from(rng.below(rounds_per_day)).unwrap_or(0);
                let magnitude = i64::try_from(1 + rng.below(MAX_JUMP_MS)).unwrap_or(1);
                let delta_ms = if rng.chance(500) {
                    magnitude
                } else {
                    -magnitude
                };
                events.push(Planned {
                    day,
                    round,
                    what: Disturbance::ClockJump { node, delta_ms },
                });
            }
            // The hub restarts once every five days, on average.
            if rng.chance(200) {
                events.push(Planned {
                    day,
                    round: u32::try_from(rng.below(rounds_per_day)).unwrap_or(0),
                    what: Disturbance::HubRestart,
                });
            }
        }
        events.sort_by_key(|event| (event.day, event.round));
        Self {
            settings: settings.clone(),
            schedule,
            events,
        }
    }

    /// The nodes planned unreachable at the absolute round `round`.
    #[must_use]
    pub fn down_at(&self, round: u64) -> BTreeSet<usize> {
        let rounds_per_day = u64::from(self.settings.rounds_per_day.max(1));
        let mut down = BTreeSet::new();
        for event in &self.events {
            if let Disturbance::Outage { node, rounds } = &event.what {
                let start = u64::from(event.day) * rounds_per_day + u64::from(event.round);
                if (start..start + rounds).contains(&round) {
                    down.insert(*node);
                }
            }
        }
        down
    }

    /// The plan as text: what a replay prints before it runs.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = format!(
            "plan SEED={} shape={} days={} nodes={} rounds/day={} events={}\n",
            self.settings.seed,
            self.settings.shape.name(),
            self.settings.days,
            self.settings.nodes,
            self.settings.rounds_per_day,
            self.events.len()
        );
        out.push_str(&self.schedule.describe());
        for event in &self.events {
            out.push_str(&format!(
                "day {:02} round {:03} {}\n",
                event.day,
                event.round,
                event.what.describe()
            ));
        }
        out
    }
}

/// A node index below `nodes`.
fn pick_node(rng: &mut Rng, nodes: usize) -> usize {
    usize::try_from(rng.below(u64::try_from(nodes).unwrap_or(1))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn many() -> Vec<SoakSettings> {
        (0..12)
            .flat_map(|seed| [SoakSettings::smoke(seed), SoakSettings::full(seed)])
            .collect()
    }

    #[test]
    fn the_same_seed_gives_the_same_plan() {
        for settings in many() {
            assert_eq!(
                SoakPlan::generate(&settings),
                SoakPlan::generate(&settings),
                "SEED={}",
                settings.seed
            );
        }
        assert_ne!(
            SoakPlan::generate(&SoakSettings::full(1)),
            SoakPlan::generate(&SoakSettings::full(2))
        );
    }

    #[test]
    fn every_outage_heals_before_the_last_day_ends() {
        for settings in many() {
            let rounds_per_day = u64::from(settings.rounds_per_day);
            let plan = SoakPlan::generate(&settings);
            for event in &plan.events {
                if let Disturbance::Outage { rounds, .. } = &event.what {
                    let start = u64::from(event.day) * rounds_per_day + u64::from(event.round);
                    assert!(*rounds >= 1);
                    assert!(
                        start + rounds <= settings.total_rounds(),
                        "SEED={} an outage outlives the run",
                        settings.seed
                    );
                }
            }
        }
    }

    #[test]
    fn no_more_than_the_configured_share_is_down_at_once() {
        for settings in many() {
            let plan = SoakPlan::generate(&settings);
            let cap = settings.max_down();
            for round in 0..settings.total_rounds() {
                assert!(
                    plan.down_at(round).len() <= cap,
                    "SEED={} round {round}: more than {cap} nodes down",
                    settings.seed
                );
            }
        }
    }

    #[test]
    fn the_release_never_upgrades_a_node_on_the_day_it_is_restored() {
        for settings in many() {
            let plan = SoakPlan::generate(&settings);
            for event in &plan.events {
                if let Disturbance::Restore { node } = &event.what {
                    assert_ne!(
                        plan.schedule.upgrade_day[*node],
                        Some(event.day),
                        "SEED={} node-{node} is restored and upgraded on day {}",
                        settings.seed,
                        event.day
                    );
                }
            }
        }
    }

    #[test]
    fn events_inside_a_day_are_sorted_by_time() {
        for settings in many() {
            let plan = SoakPlan::generate(&settings);
            let keys: Vec<(u32, u32)> = plan.events.iter().map(|e| (e.day, e.round)).collect();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            assert_eq!(keys, sorted, "SEED={}", settings.seed);
        }
    }

    #[test]
    fn the_mix_is_really_a_mix() {
        let plan = SoakPlan::generate(&SoakSettings::full(7));
        let has = |f: fn(&Disturbance) -> bool| plan.events.iter().any(|e| f(&e.what));
        assert!(has(|d| matches!(d, Disturbance::Outage { .. })));
        assert!(has(|d| matches!(d, Disturbance::Flap { .. })));
        assert!(has(|d| matches!(d, Disturbance::Restore { .. })));
        assert!(has(|d| matches!(d, Disturbance::ClockJump { .. })));
        for event in &plan.events {
            if let Disturbance::ClockJump { delta_ms, .. } = &event.what {
                assert!(delta_ms.unsigned_abs() <= MAX_JUMP_MS);
            }
        }
    }
}
