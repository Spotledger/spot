//! The soak driver: a plan, a world, a release model and the daily check.
//!
//! One seed drives everything. The world's own virtual clock is the only clock; the plan's draws
//! and the world's draws come from the suite's seeded generator; nothing sleeps, reads a wall
//! clock or opens a socket. The trace holds kinds, nodes, positions and counts only, never record
//! content.

use spot_sync_bootstrap::{StartMode, StartReason};
use spot_sync_chaos::{ChaosError, Edge, Step, World, WorldConfig};

use super::invariants::{RestoreCount, Violation, check_converged, check_day};
use super::plan::{Disturbance, SoakPlan, SoakSettings};
use super::release::ReleaseModel;

/// How many trailing trace lines a failure prints.
const TRACE_TAIL: usize = 60;

/// What a finished run counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoakReport {
    /// The seed.
    pub seed: u64,
    /// A digest of the whole trace (the driver's lines and the world's transcript).
    pub digest: u64,
    /// Rounds run, the final convergence phase included.
    pub rounds: usize,
    /// Rounds the final convergence phase took.
    pub settle_rounds: usize,
    /// Nodes restored from a backup (each detected and bootstrapped).
    pub restores: usize,
    /// Restores skipped because the node had not acknowledged enough to be restored behind.
    pub restores_skipped: usize,
    /// Outages, flaps, clock jumps and hub restarts the plan held.
    pub outages: usize,
    /// Link flaps.
    pub flaps: usize,
    /// Clock jumps.
    pub jumps: usize,
    /// Hub restarts the plan held (not simulated, see `tests/soak.rs`).
    pub hub_restarts: usize,
    /// Records held for nodes at release N when the run ended.
    pub held_at_end: usize,
    /// Upgrade offers made to the too-old node.
    pub upgrade_offers: u64,
}

/// A failed run: where, which invariant and enough to replay it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoakFailure {
    /// The seed.
    pub seed: u64,
    /// The shape's name, for the replay line.
    pub shape: &'static str,
    /// The day it failed on (the number of days for the final phase).
    pub day: u32,
    /// The invariant.
    pub invariant: &'static str,
    /// What was seen.
    pub detail: String,
    /// The last lines of the driver's trace and the world's transcript.
    pub trace_tail: Vec<String>,
}

impl SoakFailure {
    /// The one-line replay command, led by `SEED=<n>`.
    #[must_use]
    pub fn replay_line(&self) -> String {
        format!(
            "SEED={} replay: SEED={} SPOT_SOAK_SHAPE={} cargo test -p spot-sync-chaos --test soak \
             soak_replay -- --ignored --nocapture",
            self.seed, self.seed, self.shape
        )
    }

    /// The failure as text: replay line, the first violated invariant and the trace tail.
    #[must_use]
    pub fn report(&self) -> String {
        format!(
            "{}\n  day {}: invariant `{}` violated: {}\n--- last trace lines ---\n{}",
            self.replay_line(),
            self.day,
            self.invariant,
            self.detail,
            self.trace_tail.join("\n")
        )
    }
}

/// A 64-bit FNV-1a digest, so two runs can be compared without keeping both traces.
fn fnv(lines: impl Iterator<Item = impl AsRef<str>>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in lines {
        for byte in line.as_ref().bytes().chain(std::iter::once(b'\n')) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

struct Driver<'a> {
    plan: &'a SoakPlan,
    world: World,
    release: ReleaseModel,
    trace: Vec<String>,
    /// Per node, the absolute round it is unreachable until (exclusive).
    down_until: Vec<u64>,
    heal_next: Vec<usize>,
    restores: RestoreCount,
    skipped: usize,
    day: u32,
}

impl Driver<'_> {
    fn fail(&self, invariant: &'static str, detail: String) -> SoakFailure {
        let mut tail: Vec<String> = self
            .world
            .transcript()
            .iter()
            .rev()
            .take(TRACE_TAIL)
            .rev()
            .cloned()
            .collect();
        tail.push("--- driver ---".to_owned());
        tail.extend(self.trace.iter().rev().take(TRACE_TAIL).rev().cloned());
        SoakFailure {
            seed: self.plan.settings.seed,
            shape: self.plan.settings.shape.name(),
            day: self.day,
            invariant,
            detail,
            trace_tail: tail,
        }
    }

    fn world_error(&self, error: &ChaosError) -> SoakFailure {
        self.fail("the world refused", error.to_string())
    }

    fn violated(&self, violation: Violation) -> SoakFailure {
        self.fail(violation.invariant, violation.detail)
    }

    /// Applies one planned disturbance at the absolute round `abs`.
    fn apply(&mut self, abs: u64, what: &Disturbance) -> Result<(), SoakFailure> {
        self.trace.push(format!(
            "day {:02} round {abs:05} {}",
            self.day,
            what.describe()
        ));
        match what {
            Disturbance::Outage { node, rounds } => {
                self.down_until[*node] = self.down_until[*node].max(abs + rounds);
            }
            Disturbance::Flap { node, cuts } => {
                for (step, edge) in cuts {
                    self.world.cut(*node, *step, *edge);
                }
                self.heal_next.push(*node);
            }
            Disturbance::ClockJump { node, delta_ms } => {
                self.world.jump(*node, *delta_ms);
            }
            Disturbance::HubRestart => {}
            Disturbance::Restore { node } => self.restore(abs, *node)?,
        }
        Ok(())
    }

    /// Restores a node from a backup older than its last acknowledgement and lets it handshake:
    /// the hub must see a stale history and bootstrap it, not continue it.
    fn restore(&mut self, abs: u64, node: usize) -> Result<(), SoakFailure> {
        if abs < self.down_until[node] {
            // Planned for a moment the node was up; an outage drawn later for the same node may
            // overlap. A restore needs a reachable node, so it is skipped, and counted.
            self.skipped += 1;
            return Ok(());
        }
        let acked = self.world.node_reported_position(node).unwrap_or(0);
        if acked < 2 {
            self.skipped += 1;
            return Ok(());
        }
        // Strictly older than what the node acknowledged: 1 ..= acked - 1.
        let position = self.world.draw_below(acked - 1) + 1;
        self.world
            .restore_node(node, position)
            .map_err(|error| self.world_error(&error))?;
        self.restores.restores += 1;
        let report = self
            .world
            .start_session(node)
            .map_err(|error| self.world_error(&error))?;
        let detected = matches!(
            report.mode,
            StartMode::Bootstrap {
                reason: StartReason::StaleGeneration { .. },
                ..
            }
        );
        if detected && report.bootstrapped {
            self.restores.bootstrapped += 1;
        }
        self.trace.push(format!(
            "node-{node} restored to {position} (had acked {acked}): detected {detected} \
             bootstrapped {} chunks {} installed {}",
            report.bootstrapped, report.chunks, report.installed
        ));
        Ok(())
    }

    /// One round: heal last round's flaps, end finished outages, keep the down nodes down, go.
    fn round(&mut self, abs: u64) -> Result<(), SoakFailure> {
        for node in std::mem::take(&mut self.heal_next) {
            if abs >= self.down_until[node] {
                self.world.heal(node);
            }
        }
        for node in 0..self.down_until.len() {
            if self.down_until[node] == abs {
                self.world.heal(node);
            }
            if abs < self.down_until[node] {
                // Unreachable: the node cannot ask, acknowledge or report.
                self.world.cut(node, Step::Offer, Edge::Before);
                self.world.cut(node, Step::Ack, Edge::Before);
                self.world.cut(node, Step::Nack, Edge::Before);
            }
        }
        self.world
            .round()
            .map(|_| ())
            .map_err(|error| self.world_error(&error))
    }
}

/// Runs the plan of `settings`.
///
/// # Errors
/// The first invariant that does not hold, with its replay line.
pub fn run(settings: &SoakSettings) -> Result<SoakReport, SoakFailure> {
    run_plan(&SoakPlan::generate(settings))
}

/// Runs `plan`.
///
/// # Errors
/// The first invariant that does not hold, with its replay line.
pub fn run_plan(plan: &SoakPlan) -> Result<SoakReport, SoakFailure> {
    let settings = &plan.settings;
    let early = |what: String| SoakFailure {
        seed: settings.seed,
        shape: settings.shape.name(),
        day: 0,
        invariant: "the world could not be built",
        detail: what,
        trace_tail: Vec::new(),
    };
    let world = World::build(WorldConfig {
        nodes: settings.nodes,
        records: settings.records,
        // One wave of records per round for the whole run: the steady flow of documents.
        versions: settings.total_rounds(),
        batch: 16,
        tick_ms: settings.tick_ms(),
        narrow_node: (settings.nodes > 3).then_some(1),
        narrow_entities: vec!["Customer".to_owned()],
        ..WorldConfig::seeded(settings.seed)
    })
    .map_err(|error| early(error.to_string()))?;
    let mut driver = Driver {
        plan,
        world,
        release: ReleaseModel::new(settings, plan.schedule.clone()),
        trace: vec![plan.describe()],
        down_until: vec![0; settings.nodes],
        heal_next: Vec::new(),
        restores: RestoreCount::default(),
        skipped: 0,
        day: 0,
    };
    let rounds_per_day = u64::from(settings.rounds_per_day);
    let mut next_event = 0;
    for day in 0..settings.days {
        driver.day = day;
        for round_in_day in 0..settings.rounds_per_day {
            let abs = u64::from(day) * rounds_per_day + u64::from(round_in_day);
            while let Some(event) = plan.events.get(next_event) {
                if event.day != day || event.round != round_in_day {
                    break;
                }
                driver.apply(abs, &event.what)?;
                next_event += 1;
            }
            driver.round(abs)?;
        }
        let end = (u64::from(day) + 1) * rounds_per_day;
        let down_until = driver.down_until.clone();
        let stats = driver
            .release
            .step_day(day, &|node| end >= down_until[node])
            .map_err(|detail| driver.fail("rolling release upgrade", detail))?;
        driver.trace.push(format!(
            "day {day:02} end: restores {} release upgraded {} released {} held {} delivered {} offers {}",
            driver.restores.restores,
            stats.upgraded,
            stats.released,
            stats.held,
            stats.delivered,
            stats.offers
        ));
        check_day(&driver.world, driver.restores).map_err(|v| driver.violated(v))?;
    }

    // The final convergence phase: everyone reachable, no new documents once the plan's waves
    // are written, until the world is quiet within a bounded number of rounds.
    driver.day = settings.days;
    for node in 0..settings.nodes {
        driver.down_until[node] = 0;
        driver.world.heal(node);
    }
    let bound = usize::try_from(rounds_per_day).unwrap_or(48) * 20;
    let settle_rounds = driver
        .world
        .settle(bound)
        .map_err(|error| driver.world_error(&error))?;
    check_converged(&driver.world).map_err(|v| driver.violated(v))?;
    check_day(&driver.world, driver.restores).map_err(|v| driver.violated(v))?;
    driver
        .release
        .finish()
        .map_err(|detail| driver.fail("rolling release upgrade", detail))?;

    let count = |f: fn(&Disturbance) -> bool| plan.events.iter().filter(|e| f(&e.what)).count();
    let digest = fnv(driver
        .trace
        .iter()
        .map(String::as_str)
        .chain(driver.world.transcript().iter().map(String::as_str)));
    Ok(SoakReport {
        seed: settings.seed,
        digest,
        rounds: driver.world.rounds(),
        settle_rounds,
        restores: driver.restores.restores,
        restores_skipped: driver.skipped,
        outages: count(|d| matches!(d, Disturbance::Outage { .. })),
        flaps: count(|d| matches!(d, Disturbance::Flap { .. })),
        jumps: count(|d| matches!(d, Disturbance::ClockJump { .. })),
        hub_restarts: count(|d| matches!(d, Disturbance::HubRestart)),
        held_at_end: driver.release.held_total(),
        upgrade_offers: driver.release.upgrade_offers(),
    })
}
